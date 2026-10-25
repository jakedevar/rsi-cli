//! Compare exact committed guard identities over one immutable candidate tree.
//! Historical source is data: only blob-authenticated Rust adapters may run.
use crate::{
    Ctx,
    re::{self, Re},
    target_reclaim::{self, HistoricalVersion},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{LazyLock, atomic::AtomicUsize},
    time::{Duration, Instant},
};
pub const DECLARATIONS: &str = "scripts/guard-regressions-intended.json";
pub const ADAPTER_BUDGET: Duration = Duration::from_secs(120);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub path: String,
    pub line: usize,
    pub finding: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loss {
    pub guard: String,
    pub base_guard: String,
    pub head_guard: Option<String>,
    pub finding: Finding,
    pub intended: bool,
}
impl Loss {
    pub fn declaration(&self) -> Value {
        json!({"guard":self.guard,"base_guard":self.base_guard,"head_guard":self.head_guard,"path":self.finding.path,"line":self.finding.line,"finding":self.finding.finding})
    }
}
pub fn declaration_template(loss: &Loss) -> String {
    let mut value = loss.declaration();
    value["reason"] = json!("REPLACE with reviewable reason");
    let mut keys = vec![
        "guard",
        "base_guard",
        "head_guard",
        "path",
        "line",
        "finding",
        "reason",
    ];
    keys.sort_unstable();
    // json.dumps(sort_keys=True) uses spaces and ensure_ascii by default.
    let ascii = |value: &Value| match value {
        Value::String(text) => crate::os_escape::json_string(text),
        other => other.to_string(),
    };
    format!(
        "{{{}}}",
        keys.into_iter()
            .map(|k| format!("{}: {}", ascii(&json!(k)), ascii(&value[k])))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
#[derive(Clone, Debug, Default)]
pub struct Comparison {
    pub losses: Vec<Loss>,
    pub unsupported: Vec<String>,
    pub base: String,
    pub head: String,
}
#[derive(Clone, Debug)]
pub struct Blob {
    pub oid: String,
    pub bytes: Vec<u8>,
}
pub type Snapshot = BTreeMap<String, Blob>;
fn command(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        c.env_remove(key);
    }
    c
}
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = command(repo)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().into());
    }
    Ok(out.stdout)
}
fn resolve(repo: &Path, value: &str) -> Result<String, String> {
    let bytes = git(
        repo,
        &["rev-parse", "--verify", &format!("{value}^{{commit}}")],
    )?;
    let sha = String::from_utf8(bytes)
        .map_err(|e| e.to_string())?
        .trim()
        .to_string();
    if !hash(&sha) {
        return Err("invalid resolved commit".into());
    }
    Ok(sha)
}
fn hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn safe_path(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('/') && !s.split('/').any(|p| p == "..") && !s.contains('\0')
}
pub fn guard_path(path: &str) -> bool {
    static GUARD: LazyLock<Re> = LazyLock::new(|| Re::new(r"^(scripts|tools)/check[-_][^/]+\.py$"));
    GUARD.is_match(path) || crate::GUARDS.iter().any(|(_, _, p)| *p == path)
}
fn source_path(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or(""),
        "" | "rs" | "py" | "sh" | "js" | "mjs" | "cjs" | "ts" | "tsx"
    )
}
/// Read regular committed blobs directly, ignoring export attributes, symlinks,
/// gitlinks and every byte of mutable worktree state. No executable is exported.
pub fn snapshot(repo: &Path, head: &str) -> Result<Snapshot, String> {
    let tree = git(repo, &["ls-tree", "-rz", "--full-tree", head])?;
    let mut files = Vec::new();
    for record in tree.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| "malformed head tree".to_string())?;
        let metadata = std::str::from_utf8(&record[..tab]).map_err(|e| e.to_string())?;
        let fields: Vec<&str> = metadata.split_whitespace().collect();
        let path = crate::os_escape::decode(&record[tab + 1..])?;
        let path = path.as_str();
        if !safe_path(path) {
            return Err("unsafe head tree path".into());
        }
        if fields.len() != 3 {
            return Err("malformed head tree metadata".into());
        }
        if fields[1] == "blob"
            && matches!(fields[0], "100644" | "100755")
            && (source_path(path) || path == DECLARATIONS)
        {
            files.push((path.to_string(), fields[2].to_string()));
        }
    }
    if files.is_empty() {
        return Ok(Snapshot::new());
    }
    let mut child = command(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| "missing object batch stdin".to_string())?;
    let request = files
        .iter()
        .map(|(_, oid)| format!("{oid}\n"))
        .collect::<String>();
    // Write concurrently: a full object-output pipe must not block input.
    let writer = std::thread::spawn(move || input.write_all(request.as_bytes()));
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    writer
        .join()
        .map_err(|_| "object batch writer failed".to_string())?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let mut at = 0;
    let mut result = Snapshot::new();
    for (path, oid) in files {
        let end = output.stdout[at..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| at + n)
            .ok_or_else(|| "truncated head blob header".to_string())?;
        let header = std::str::from_utf8(&output.stdout[at..end]).map_err(|e| e.to_string())?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        if fields.len() != 3 || fields[0] != oid || fields[1] != "blob" {
            return Err("unexpected head blob".into());
        }
        let size: usize = fields[2]
            .parse()
            .map_err(|_| "invalid head blob size".to_string())?;
        at = end + 1;
        let end = at
            .checked_add(size)
            .ok_or_else(|| "head blob size overflow".to_string())?;
        if output.stdout.get(end) != Some(&b'\n') {
            return Err("truncated head blob".into());
        }
        result.insert(
            path,
            Blob {
                oid,
                bytes: output.stdout[at..end].to_vec(),
            },
        );
        at = end + 1;
    }
    if at != output.stdout.len() {
        return Err("unexpected object batch tail".into());
    }
    Ok(result)
}
fn blob(repo: &Path, commit: &str, path: &str) -> Result<Option<Blob>, String> {
    let tree = git(
        repo,
        &["ls-tree", "-z", commit, "--", &format!(":(literal){path}")],
    )?;
    let Some(record) = tree.split(|b| *b == 0).find(|r| !r.is_empty()) else {
        return Ok(None);
    };
    let tab = record
        .iter()
        .position(|b| *b == b'\t')
        .ok_or_else(|| "malformed guard tree entry".to_string())?;
    if &record[tab + 1..] != path.as_bytes() {
        return Err("unexpected guard path".into());
    }
    let meta = std::str::from_utf8(&record[..tab]).map_err(|e| e.to_string())?;
    let fields: Vec<&str> = meta.split_whitespace().collect();
    if fields.len() != 3 || fields[1] != "blob" || !matches!(fields[0], "100644" | "100755") {
        return Err(format!("{path}: guard is not a regular committed blob"));
    }
    Ok(Some(Blob {
        oid: fields[2].into(),
        bytes: git(repo, &["cat-file", "blob", fields[2]])?,
    }))
}
fn validate_finding(path: &str, line: usize, finding: &str) -> Result<Finding, String> {
    if !safe_path(path) || line == 0 || finding.trim().is_empty() {
        return Err(format!("invalid finding: {path}:{line}: {finding}"));
    }
    Ok(Finding {
        path: path.into(),
        line,
        finding: finding.into(),
    })
}
fn test_path(path: &str) -> bool {
    let p = Path::new(path);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    path.split('/')
        .any(|s| matches!(s, "tests" | "fixtures" | "__tests__") || s.ends_with("_tests"))
        || stem == "tests"
        || name.starts_with("test_")
        || ["_test", "_tests", ".test", ".spec"]
            .iter()
            .any(|s| stem.ends_with(s))
}
/// Exact legacy line exclusions, including shared production lines.
pub fn rust_test_lines(source: &str) -> Result<BTreeSet<usize>, String> {
    let code = crate::rust_source::mask(source, "guard regression source")?;
    let mut hidden = BTreeSet::new();
    let attrs =
        Re::new(r"#!?\[\s*cfg\(\s*test\s*\)\s*\]|#\[\s*(?:test|tokio::test)(?:\([^]]*\))?\s*\]");
    for attr in attrs.find_all(&code) {
        let start = re::start(&attr);
        let mut start_line = crate::rust_source::line(source, start);
        if re::group(&attr, 0).starts_with("#!") {
            let prefix = &code[..start];
            if prefix.matches('{').count() == prefix.matches('}').count() {
                return Ok((1..=source.matches('\n').count() + 1).collect());
            }
            continue;
        }
        let line_start = code[..start].rfind('\n').map_or(0, |n| n + 1);
        if !code[line_start..start].trim().is_empty() {
            start_line += 1;
        }
        let mut at = re::end(&attr);
        while let Some(extra) = Re::new(r"^\s*#\[[^\]]*\]").search(&code[at..]) {
            at += re::end(&extra);
        }
        let body = code[at..]
            .find([';', '{'])
            .ok_or_else(|| "cannot locate Rust test item".to_string())?
            + at;
        let end = if code.as_bytes()[body] == b'{' {
            crate::rust_source::closing(&code, body)
                .map_err(|_| "unterminated Rust test item".to_string())?
        } else {
            body + 1
        };
        let mut end_line = crate::rust_source::line(source, end);
        let eol = code[end..].find('\n').map_or(code.len(), |n| end + n);
        if !code[end..eol].trim().is_empty() {
            end_line = end_line.saturating_sub(1);
        }
        hidden.extend(start_line..=end_line);
    }
    Ok(hidden)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterKind {
    Entries,
    Stdout,
}
pub fn parse_stdout(exit: i32, stdout: &str, stderr: &str) -> Result<BTreeSet<Finding>, String> {
    if !matches!(exit, 0 | 1) {
        return Err(format!("guard exited {exit}: {stderr}"));
    }
    let diagnostic = Re::new(r"^(.+?):([1-9][0-9]*):\s*(\S.*)$");
    let mut findings = BTreeSet::new();
    for line in crate::py::splitlines(&format!("{stdout}{stderr}")) {
        let m = diagnostic
            .search(line)
            .ok_or_else(|| format!("unstable guard diagnostic: {}", crate::py::repr(line)))?;
        let number = re::group(&m, 2)
            .parse()
            .map_err(|_| "invalid finding line".to_string())?;
        findings.insert(validate_finding(
            re::group(&m, 1),
            number,
            re::group(&m, 3),
        )?);
    }
    if exit == 1 && findings.is_empty() {
        return Err("guard exited 1 without findings".into());
    }
    Ok(findings)
}
fn advertised(source: &[u8]) -> Option<AdapterKind> {
    let text = std::str::from_utf8(source).ok()?;
    if Re::new(r"(?m)^(?:#|//) guard-regressions: stdout\s*$").is_match(text) {
        Some(AdapterKind::Stdout)
    } else if Re::new(r"(?m)^def entries\(\s*text\s*\)\s*:").is_match(text)
        || Re::new(r"(?m)^pub fn (?:historical_)?entries\(").is_match(text)
    {
        Some(AdapterKind::Entries)
    } else {
        None
    }
}
/// Caller-owned trusted adapters are keyed by the exact guard path and blob.
/// The implementation must not load a worktree checker or interpret Python.
pub trait HistoricalAdapters {
    fn collect(
        &self,
        guard: &str,
        version: &Blob,
        commit: &str,
        kind: AdapterKind,
        head: &Snapshot,
        deadline: Instant,
    ) -> Result<BTreeSet<Finding>, String>;
}
pub struct BuiltinAdapters;
impl HistoricalAdapters for BuiltinAdapters {
    fn collect(
        &self,
        guard: &str,
        version: &Blob,
        _commit: &str,
        kind: AdapterKind,
        head: &Snapshot,
        deadline: Instant,
    ) -> Result<BTreeSet<Finding>, String> {
        // These immutable Git blobs exhaust the target checker revisions in
        // this source's history. Policy-only nextest changes share detectors.
        let mode = match (guard, version.oid.as_str(), kind) {
            (
                "scripts/check-target-reclaim.py",
                "81faa4333945c22c8b33dd6eecf12532e5a72e1e"
                | "fb0d7260ca75130409700cbd1e8f8280bc4059c7",
                AdapterKind::Entries,
            ) => HistoricalVersion::Global,
            (
                "scripts/check-target-reclaim.py",
                "350d231e47ae13c534efc991aadfe9dc962c6912",
                AdapterKind::Entries,
            ) => HistoricalVersion::Scoped,
            (
                "scripts/check-target-reclaim.py",
                "b27d79cf21809643b6bdef22701cf2d0e0d78602"
                | "1b56d6ccfbab271b395038724695e7f9f5fdf817",
                AdapterKind::Entries,
            ) => HistoricalVersion::Conservative,
            _ => {
                return Err(format!(
                    "{guard}: authenticated historical adapter required for blob {} ({kind:?}); supported checker was not skipped",
                    version.oid
                ));
            }
        };
        if advertised(&version.bytes) != Some(kind) {
            return Err(format!(
                "{guard}: needs entries(text) or '# guard-regressions: stdout'; see docs/guard-regressions.md"
            ));
        }
        let mut findings = BTreeSet::new();
        for (path, blob) in head {
            if Instant::now() >= deadline {
                return Err(format!(
                    "{guard}: comparison exceeded 120-second adapter deadline"
                ));
            }
            if !source_path(path) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&blob.bytes) else {
                continue;
            };
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            for (line, finding) in target_reclaim::historical_entries(&text, mode) {
                findings.insert(validate_finding(path, line, &finding)?);
            }
        }
        Ok(findings)
    }
}
/// The CLI subcommand of a native guard module, by its source path.
pub fn native_cli(path: &str) -> Option<&'static str> {
    crate::GUARDS
        .iter()
        .find(|(_, _, source)| *source == path)
        .map(|(name, _, _)| *name)
}
/// Supplies the trusted prebuilt `rsi-repo-guard` of one exact commit. Building
/// and authenticating it belongs to the trusted materialization (hub #3621);
/// this crate never compiles historical source on its own.
pub trait BinaryResolver {
    fn binary(&self, commit: &str) -> Result<PathBuf, String>;
}
/// An explicit `COMMIT=PATH` table. A commit absent from it fails closed.
#[derive(Debug, Default)]
pub struct BinaryTable(BTreeMap<String, PathBuf>);
impl BinaryTable {
    pub fn parse(items: &[String]) -> Result<Self, String> {
        let mut table = BTreeMap::new();
        for item in items {
            let (commit, path) = item
                .split_once('=')
                .filter(|(commit, path)| hash(commit) && !path.is_empty())
                .ok_or_else(|| format!("--historical-binary needs FULL_COMMIT=PATH, got {item}"))?;
            if table
                .insert(commit.to_string(), PathBuf::from(path))
                .is_some()
            {
                return Err(format!("--historical-binary lists {commit} twice"));
            }
        }
        Ok(Self(table))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl BinaryResolver for BinaryTable {
    fn binary(&self, commit: &str) -> Result<PathBuf, String> {
        self.0.get(commit).cloned().ok_or_else(|| {
            format!("no trusted rsi-repo-guard is materialized for commit {commit} (hub #3621)")
        })
    }
}
/// Write every regular committed blob of `head` under `dir` (export attributes,
/// symlinks, gitlinks and the worktree are ignored), streaming one object at a
/// time so a large tree is never held in memory.
pub fn export_tree(repo: &Path, head: &str, dir: &Path) -> Result<(), String> {
    let tree = git(repo, &["ls-tree", "-rz", "--full-tree", head])?;
    let mut files = Vec::new();
    for record in tree.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| "malformed head tree".to_string())?;
        let metadata = std::str::from_utf8(&record[..tab]).map_err(|e| e.to_string())?;
        let fields: Vec<&str> = metadata.split_whitespace().collect();
        if fields.len() != 3 {
            return Err("malformed head tree metadata".into());
        }
        let relative = crate::os_escape::path_from_bytes(&record[tab + 1..]);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("unsafe head tree path".into());
        }
        if fields[1] == "blob" && matches!(fields[0], "100644" | "100755") {
            files.push((relative, fields[0] == "100755", fields[2].to_string()));
        }
    }
    if files.is_empty() {
        return Ok(());
    }
    let mut child = command(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| "missing object batch stdin".to_string())?;
    let request = files
        .iter()
        .map(|(_, _, oid)| format!("{oid}\n"))
        .collect::<String>();
    let writer = std::thread::spawn(move || input.write_all(request.as_bytes()));
    let mut output = std::io::BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| "missing object batch stdout".to_string())?,
    );
    let outcome = (|| -> Result<(), String> {
        use std::io::{BufRead, Read};
        for (relative, executable, oid) in &files {
            let mut header = String::new();
            output.read_line(&mut header).map_err(|e| e.to_string())?;
            let fields: Vec<&str> = header.split_whitespace().collect();
            if fields.len() != 3 || fields[0] != oid || fields[1] != "blob" {
                return Err("unexpected head blob".into());
            }
            let size: u64 = fields[2]
                .parse()
                .map_err(|_| "invalid head blob size".to_string())?;
            let destination = dir.join(relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut file = std::fs::File::create(&destination).map_err(|e| e.to_string())?;
            let copied = std::io::copy(&mut (&mut output).take(size), &mut file)
                .map_err(|e| e.to_string())?;
            if copied != size {
                return Err("truncated head blob".into());
            }
            let mut newline = [0u8; 1];
            output.read_exact(&mut newline).map_err(|e| e.to_string())?;
            if newline[0] != b'\n' {
                return Err("truncated head blob".into());
            }
            #[cfg(unix)]
            if *executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| e.to_string())?;
            }
            #[cfg(not(unix))]
            let _ = executable;
        }
        Ok(())
    })();
    let _ = child.kill();
    let _ = child.wait();
    let _ = writer.join();
    outcome
}
/// Runs a native guard's historical version as the trusted `rsi-repo-guard` of
/// that commit (`<cli> --root <head tree>`): a separate, killable process per
/// version with its own hard deadline. Python-era blobs and every other kind
/// stay with `fallback`.
pub struct ExecutableAdapters<'a> {
    pub resolver: &'a dyn BinaryResolver,
    pub fallback: &'a dyn HistoricalAdapters,
    pub repo: &'a Path,
    /// The resolved head object id whose tree every version inspects.
    pub head: &'a str,
    pub environment: &'a BTreeMap<String, String>,
    pub cancel: &'a AtomicUsize,
    exported: RefCell<Option<tempfile::TempDir>>,
}
impl<'a> ExecutableAdapters<'a> {
    pub fn new(
        resolver: &'a dyn BinaryResolver,
        fallback: &'a dyn HistoricalAdapters,
        repo: &'a Path,
        head: &'a str,
        environment: &'a BTreeMap<String, String>,
        cancel: &'a AtomicUsize,
    ) -> Self {
        Self {
            resolver,
            fallback,
            repo,
            head,
            environment,
            cancel,
            exported: RefCell::new(None),
        }
    }
    fn root(&self) -> Result<PathBuf, String> {
        let mut exported = self.exported.borrow_mut();
        if exported.is_none() {
            let dir = crate::scratch_git::Policy::from_process(self.repo)
                .and_then(|policy| policy.tempdir("rsi-guard-regressions-"))
                .map_err(|e| format!("cannot allocate the head-tree export: {e}"))?;
            export_tree(self.repo, self.head, dir.path())?;
            *exported = Some(dir);
        }
        Ok(exported
            .as_ref()
            .map(|d| d.path().to_path_buf())
            .unwrap_or_default())
    }
}
impl HistoricalAdapters for ExecutableAdapters<'_> {
    fn collect(
        &self,
        guard: &str,
        version: &Blob,
        commit: &str,
        kind: AdapterKind,
        head: &Snapshot,
        deadline: Instant,
    ) -> Result<BTreeSet<Finding>, String> {
        let Some(cli) = native_cli(guard).filter(|_| kind == AdapterKind::Stdout) else {
            return self
                .fallback
                .collect(guard, version, commit, kind, head, deadline);
        };
        let binary = self.resolver.binary(commit)?;
        let root = self.root()?;
        let argv = vec![
            binary.to_string_lossy().into_owned(),
            cli.to_string(),
            "--root".into(),
            root.to_string_lossy().into_owned(),
        ];
        let mut output = [Vec::new(), Vec::new()];
        let outcome = crate::scoped_runner::process::stream_bytes(
            &argv,
            &root,
            self.environment,
            deadline,
            self.cancel,
            |pipe, bytes| output[pipe.min(1)].extend_from_slice(bytes),
        )
        .map_err(|e| format!("{guard}: cannot run the historical guard: {e}"))?;
        if outcome.timed_out {
            return Err(format!(
                "{guard}: comparison exceeded {}-second adapter deadline",
                ADAPTER_BUDGET.as_secs()
            ));
        }
        if let Some(signal) = outcome.cancelled {
            return Err(format!("{guard}: comparison cancelled by signal {signal}"));
        }
        if !outcome.output_complete {
            return Err(format!("{guard}: historical guard output was incomplete"));
        }
        parse_stdout(
            outcome.code,
            &String::from_utf8_lossy(&output[0]),
            &String::from_utf8_lossy(&output[1]),
        )
        .map_err(|e| format!("{guard}: {e}"))
    }
}
fn declarations(tree: &Snapshot) -> Result<BTreeSet<String>, String> {
    let data = match tree.get(DECLARATIONS) {
        Some(blob) => serde_json::from_slice::<Value>(&blob.bytes).map_err(|e| e.to_string())?,
        None => json!([]),
    };
    let rows = data
        .as_array()
        .ok_or_else(|| format!("{DECLARATIONS} must be a list"))?;
    let keys = [
        "guard",
        "base_guard",
        "head_guard",
        "path",
        "line",
        "finding",
    ];
    let mut intended = BTreeSet::new();
    for row in rows {
        let error = || {
            format!(
                "{DECLARATIONS}: each exact loss needs {} and a reason",
                keys.join(", ")
            )
        };
        let object = row.as_object().ok_or_else(error)?;
        if object.len() != 7
            || !keys.iter().all(|k| object.contains_key(*k))
            || !row["reason"].as_str().is_some_and(|r| !r.trim().is_empty())
        {
            return Err(error());
        }
        let path = row["path"].as_str().ok_or_else(error)?;
        let line = row["line"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(error)?;
        let finding = row["finding"].as_str().ok_or_else(error)?;
        validate_finding(path, line, finding)?;
        if !row["guard"].as_str().is_some_and(guard_path)
            || !row["base_guard"].as_str().is_some_and(hash)
            || !(row["head_guard"].is_null() || row["head_guard"].as_str().is_some_and(hash))
        {
            return Err(format!("{DECLARATIONS}: invalid guard path or blob hash"));
        }
        let mut identity = row.clone();
        identity.as_object_mut().ok_or_else(error)?.remove("reason");
        intended.insert(identity.to_string());
    }
    Ok(intended)
}
pub fn compare(
    repo: &Path,
    base: &str,
    head: &str,
    guards: Option<&[String]>,
) -> Result<Comparison, String> {
    compare_with(repo, base, head, guards, &BuiltinAdapters)
}
pub fn compare_with(
    repo: &Path,
    base: &str,
    head: &str,
    guards: Option<&[String]>,
    adapters: &dyn HistoricalAdapters,
) -> Result<Comparison, String> {
    let base = resolve(repo, base)?;
    let head = resolve(repo, head)?;
    let tree = snapshot(repo, &head)?;
    let intended = declarations(&tree)?;
    let changed = match guards {
        Some(guards) => guards.to_vec(),
        None => git(
            repo,
            &["diff", "--name-only", "--no-renames", "-z", &base, &head],
        )?
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        // A guard path is ASCII: a non-UTF8 changed name cannot be one (the legacy
        // checker crashed decoding it instead of ignoring it).
        .filter_map(|s| String::from_utf8(s.to_vec()).ok())
        .filter(|p| guard_path(p))
        .collect(),
    };
    let mut result = Comparison {
        base: base.clone(),
        head: head.clone(),
        ..Comparison::default()
    };
    for guard in changed.into_iter().collect::<BTreeSet<_>>() {
        if !guard_path(&guard) {
            return Err(format!("not a static guard: {guard}"));
        }
        let before = blob(repo, &base, &guard)?;
        let after = tree.get(&guard);
        let Some(before) = before else {
            continue;
        };
        if after.is_some_and(|b| b.oid == before.oid) {
            continue;
        }
        for version in std::iter::once(&before).chain(after) {
            std::str::from_utf8(&version.bytes).map_err(|e| format!("{guard}: {e}"))?;
        }
        let kinds = [
            Some(advertised(&before.bytes)),
            after.map(|a| advertised(&a.bytes)),
        ];
        let kind = if kinds.iter().any(|k| *k == Some(Some(AdapterKind::Stdout))) {
            Some(AdapterKind::Stdout)
        } else if kinds.iter().any(|k| *k == Some(Some(AdapterKind::Entries))) {
            Some(AdapterKind::Entries)
        } else {
            None
        };
        let Some(kind) = kind else {
            result.unsupported.push(guard);
            continue;
        };
        // Rehash each supported checker, binding its bytes to its committed oid
        // before selecting native semantics. A modified registration cannot
        // stand in for the original historical detector.
        for version in std::iter::once(&before).chain(after) {
            let mut child = command(repo)
                .args(["hash-object", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| "missing hash stdin".to_string())?;
            stdin.write_all(&version.bytes).map_err(|e| e.to_string())?;
            drop(stdin);
            let done = child.wait_with_output().map_err(|e| e.to_string())?;
            if !done.status.success() || String::from_utf8_lossy(&done.stdout).trim() != version.oid
            {
                return Err(format!(
                    "{guard}: committed guard blob authentication failed"
                ));
            }
        }
        let previous = adapters.collect(
            &guard,
            &before,
            &base,
            kind,
            &tree,
            Instant::now() + ADAPTER_BUDGET,
        )?;
        let current = match after {
            Some(after) => adapters.collect(
                &guard,
                after,
                &head,
                kind,
                &tree,
                Instant::now() + ADAPTER_BUDGET,
            )?,
            None => BTreeSet::new(),
        };
        for finding in previous.iter().chain(&current) {
            validate_finding(&finding.path, finding.line, &finding.finding)?;
        }
        let mut hidden = BTreeMap::new();
        for finding in previous.difference(&current) {
            if test_path(&finding.path) {
                continue;
            }
            if finding.path.ends_with(".rs") {
                if !hidden.contains_key(&finding.path) {
                    let text = tree
                        .get(&finding.path)
                        .ok_or_else(|| "finding outside committed head tree".to_string())?;
                    hidden.insert(
                        finding.path.clone(),
                        rust_test_lines(
                            std::str::from_utf8(&text.bytes).map_err(|e| e.to_string())?,
                        )?,
                    );
                }
                if hidden[&finding.path].contains(&finding.line) {
                    continue;
                }
            }
            let mut loss = Loss {
                guard: guard.clone(),
                base_guard: before.oid.clone(),
                head_guard: after.map(|a| a.oid.clone()),
                finding: finding.clone(),
                intended: false,
            };
            loss.intended = intended.contains(&loss.declaration().to_string());
            result.losses.push(loss);
        }
    }
    Ok(result)
}
pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // --guard is repeatable; the shared parser intentionally handles only
    // scalar flags, so collect this one option before parsing the remainder.
    let mut guards = Vec::new();
    let mut binaries = Vec::new();
    let mut remaining = Vec::new();
    let mut at = 0;
    while at < args.len() {
        let repeatable = ["--guard", "--historical-binary"]
            .into_iter()
            .find(|name| args[at] == *name);
        if let Some(name) = repeatable {
            at += 1;
            let Some(value) = args.get(at) else {
                let _ = writeln!(err, "guard regressions: {name} requires a value");
                return 2;
            };
            if name == "--guard" {
                guards.push(value.clone());
            } else {
                binaries.push(value.clone());
            }
        } else if let Some(value) = args[at].strip_prefix("--guard=") {
            guards.push(value.into());
        } else if let Some(value) = args[at].strip_prefix("--historical-binary=") {
            binaries.push(value.into());
        } else {
            remaining.push(args[at].clone());
        }
        at += 1;
    }
    let parsed = match crate::cli::parse(
        "check-guard-regressions",
        "BASE [HEAD] [--repo REPO] [--guard GUARD] [--historical-binary COMMIT=PATH]",
        &remaining,
        &[crate::cli::Opt {
            name: "--repo",
            value: true,
        }],
        &[],
        Some(2),
        err,
    ) {
        Ok(p) => p,
        Err(e) => return e.code(),
    };
    let Some(base) = parsed.positionals.first() else {
        let _ = writeln!(err, "guard regressions: BASE is required");
        return 2;
    };
    let head = parsed.positionals.get(1).map_or("HEAD", String::as_str);
    let repo = ctx.path(parsed.value("--repo").unwrap_or("."));
    let selected = if guards.is_empty() {
        None
    } else {
        Some(guards.as_slice())
    };
    let table = match BinaryTable::parse(&binaries) {
        Ok(table) => table,
        Err(error) => {
            let _ = writeln!(err, "guard regressions: {error}");
            return 2;
        }
    };
    let compared = if table.is_empty() {
        compare(&repo, base, head, selected)
    } else {
        // Native guard versions run as the trusted binaries the caller names, each a
        // killable child of its own; the head is pinned to one object id.
        (|| -> Result<Comparison, String> {
            let head = resolve(&repo, head)?;
            let environment = crate::scoped_runner::process::build_env(ctx);
            let cancel = crate::scoped_runner::process::install_signals()
                .map_err(|error| error.to_string())?;
            let adapters = ExecutableAdapters::new(
                &table,
                &BuiltinAdapters,
                &repo,
                &head,
                &environment,
                &cancel,
            );
            compare_with(&repo, base, &head, selected, &adapters)
        })()
    };
    match compared {
        Ok(result) => {
            for guard in result.unsupported {
                let _ = writeln!(
                    err,
                    "unsupported: {guard} (no entries() or stdout opt-in; not compared)"
                );
            }
            for loss in &result.losses {
                let state = if loss.intended {
                    "intended"
                } else {
                    "UNDECLARED"
                };
                let _ = writeln!(
                    out,
                    "{}: lost production finding {}:{}: {} ({state})",
                    loss.guard,
                    crate::os_escape::display(&loss.finding.path),
                    loss.finding.line,
                    loss.finding.finding
                );
                if !loss.intended {
                    let declaration = declaration_template(loss);
                    let _ = writeln!(out, "  declare in {DECLARATIONS}: {declaration}");
                }
            }
            i32::from(result.losses.iter().any(|l| !l.intended))
        }
        Err(error) => {
            let _ = writeln!(err, "guard regressions: {error}");
            2
        }
    }
}
#[cfg(test)]
#[path = "guard_regressions_tests.rs"]
mod tests;
