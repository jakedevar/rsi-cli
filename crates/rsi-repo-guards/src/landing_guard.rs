//! Accepted-source reversion detection (#363). `landing_guard_cli` is the command;
//! the lander still has to be switched to it (hub #3621 owns the trusted executable).
use crate::qa_fingerprint::sha256;
use crate::scoped_runner::process;
use crate::scratch_git::Environment;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
/// Archive exports are bigger than metadata steps: still bounded and cancellable.
const EXPORT_BOUND: Duration = Duration::from_secs(300);

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Shared overall deadline applies to Git and CPU matching, not just each step.
/// Metadata commands also have a 45s step bound and an 8MiB cap per pipe.
pub struct Repository<'a> {
    root: PathBuf,
    environment: Environment,
    deadline: Instant,
    cancel: &'a AtomicUsize,
}
impl<'a> Repository<'a> {
    pub fn new(
        root: &Path,
        base: &Environment,
        deadline: Instant,
        cancel: &'a AtomicUsize,
    ) -> io::Result<Self> {
        let root = root.canonicalize()?;
        let mut environment: Environment = base
            .iter()
            .filter(|(k, _)| {
                let key = k.to_string_lossy().to_ascii_uppercase();
                !key.starts_with("GIT_") && !key.starts_with("RSI_")
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        environment.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
        environment.insert(
            "GIT_CONFIG_GLOBAL".into(),
            if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
        );
        if let Some(parent) = root.parent() {
            environment.insert(
                "GIT_CEILING_DIRECTORIES".into(),
                std::env::join_paths([parent]).map_err(|e| error(e.to_string()))?,
            );
        }
        Ok(Self {
            root,
            environment,
            deadline,
            cancel,
        })
    }
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "landing guard cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "landing guard deadline exceeded",
            ));
        }
        Ok(())
    }
    fn git_with(
        &self,
        args: &[OsString],
        environment: &Environment,
    ) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
        self.check()?;
        let mut streams = [Vec::new(), Vec::new()];
        let mut oversized = false;
        let outcome = process::stream_git_bytes(
            &self.root,
            args,
            environment,
            self.deadline.min(Instant::now() + Duration::from_secs(45)),
            self.cancel,
            |index, bytes| {
                if streams[index].len().saturating_add(bytes.len()) > 8 * 1024 * 1024 {
                    oversized = true;
                } else {
                    streams[index].extend_from_slice(bytes);
                }
            },
        )?;
        self.check()?;
        if outcome.timed_out {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "landing guard Git step timed out",
            ));
        }
        if outcome.cancelled.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "landing guard Git step cancelled",
            ));
        }
        if oversized || !outcome.output_complete {
            return Err(error("incomplete or oversized landing guard Git output"));
        }
        let [stdout, stderr] = streams;
        Ok((outcome.code, stdout, stderr))
    }
    fn git_os(&self, args: &[OsString]) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
        self.git_with(args, &self.environment)
    }
    fn git(&self, args: &[&str], check: bool) -> io::Result<Vec<u8>> {
        let (code, stdout, stderr) =
            self.git_os(&args.iter().map(OsString::from).collect::<Vec<_>>())?;
        if check && code != 0 {
            return Err(error(format!(
                "git {} failed: {}",
                args[0],
                String::from_utf8_lossy(&stderr).trim()
            )));
        }
        Ok(stdout)
    }
    fn status(&self, args: &[&str]) -> io::Result<(i32, Vec<u8>)> {
        self.git_os(&args.iter().map(OsString::from).collect::<Vec<_>>())
            .map(|(code, out, _)| (code, out))
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// `git rev-parse --show-toplevel` for the opened directory.
    pub fn toplevel(&self) -> io::Result<PathBuf> {
        let output = self.git(&["rev-parse", "--show-toplevel"], true)?;
        let text =
            String::from_utf8(output).map_err(|_| error("repository top level is not UTF-8"))?;
        Ok(PathBuf::from(text.trim_end_matches(['\n', '\r'])))
    }
    pub fn rev_parse(&self, revision: &str) -> io::Result<String> {
        let output = self.git(&["rev-parse", revision], true)?;
        Ok(String::from_utf8_lossy(&output).trim().to_owned())
    }
    /// Whether `object` (`<rev>:<path>`) exists.
    pub fn exists(&self, object: &str) -> io::Result<bool> {
        Ok(self.status(&["cat-file", "-e", object])?.0 == 0)
    }
    /// True when the working tree matches HEAD with no staged, unstaged or
    /// untracked (non-ignored) change: the legacy `status --porcelain` check.
    pub fn worktree_is_clean(&self) -> io::Result<bool> {
        let changed = self.git(
            &["diff", "--no-ext-diff", "--name-only", "-z", "HEAD"],
            true,
        )?;
        let untracked = self.git(&["ls-files", "--others", "--exclude-standard", "-z"], true)?;
        Ok(changed.is_empty() && untracked.is_empty())
    }
    /// Export the committed `paths` of `revision` (a `git archive`) into `destination`,
    /// so dirty working-tree files can never hide from a static guard.
    pub fn export(&self, revision: &str, paths: &[&str], destination: &Path) -> io::Result<usize> {
        self.check()?;
        let mut args: Vec<OsString> =
            vec!["archive".into(), "--format=tar".into(), revision.into()];
        args.push("--".into());
        args.extend(paths.iter().map(OsString::from));
        let mut extractor = crate::tar_extract::Extractor::new(destination);
        let mut failure: Option<io::Error> = None;
        let mut stderr = Vec::new();
        let outcome = process::stream_git_bytes(
            &self.root,
            &args,
            &self.environment,
            self.deadline.min(Instant::now() + EXPORT_BOUND),
            self.cancel,
            |index, bytes| {
                if index == 1 {
                    if stderr.len() < 64 * 1024 {
                        stderr.extend_from_slice(bytes);
                    }
                } else if failure.is_none()
                    && let Err(e) = extractor.feed(bytes)
                {
                    failure = Some(e);
                }
            },
        )?;
        self.check()?;
        if outcome.timed_out {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "landing guard archive export timed out",
            ));
        }
        if outcome.cancelled.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "landing guard archive export cancelled",
            ));
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        if outcome.code != 0 {
            return Err(error(format!(
                "git archive failed: {}",
                String::from_utf8_lossy(&stderr).trim()
            )));
        }
        if !outcome.output_complete || !extractor.complete() {
            return Err(error("incomplete candidate archive"));
        }
        Ok(extractor.files())
    }
    pub fn commit(&self, rev: &str) -> io::Result<()> {
        if !oid(rev) {
            return Err(error("commit IDs must be full lowercase object names"));
        }
        if self
            .status(&["cat-file", "-e", &format!("{rev}^{{commit}}")])?
            .0
            != 0
        {
            return Err(error(format!("commit does not exist: {rev}")));
        }
        Ok(())
    }
    pub fn ancestor(&self, old: &str, new: &str) -> io::Result<bool> {
        Ok(self.status(&["merge-base", "--is-ancestor", old, new])?.0 == 0)
    }
    pub fn paths_changed(&self, old: &str, new: &str) -> io::Result<Vec<PathBuf>> {
        path_fields(&self.git(
            &[
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--name-only",
                "-z",
                old,
                new,
            ],
            true,
        )?)
    }
    pub fn blob(&self, rev: &str, path: &Path) -> io::Result<Option<Vec<u8>>> {
        let mut object = OsString::from(format!("{rev}:"));
        object.push(path);
        let (code, stdout, stderr) = self.git_os(&["show".into(), object])?;
        if code == 0 {
            return Ok(Some(stdout));
        }
        // Only a path the revision's tree does not list is absent; any other failure is
        // inconclusive and must refuse rather than read as "no such file".
        let (listed_code, listed, _) = self.git_os(&[
            "ls-tree".into(),
            "--name-only".into(),
            rev.into(),
            "--".into(),
            path.into(),
        ])?;
        if listed_code == 0 && listed.is_empty() {
            Ok(None)
        } else {
            Err(error(format!(
                "cannot read {rev}:{}: {}",
                path.display(),
                String::from_utf8_lossy(&stderr).trim()
            )))
        }
    }
    pub fn renamed_paths(
        &self,
        source: &str,
        candidate: &str,
    ) -> io::Result<BTreeMap<PathBuf, PathBuf>> {
        let data = self.git(
            &[
                "diff",
                "--no-ext-diff",
                "--find-renames=20%",
                "--name-status",
                "-z",
                source,
                candidate,
            ],
            true,
        )?;
        let fields: Vec<_> = data
            .split(|b| *b == 0)
            .filter(|field| !field.is_empty())
            .collect();
        let mut result = BTreeMap::new();
        let mut i = 0;
        while i < fields.len() {
            let rename = fields[i].starts_with(b"R");
            let count = if rename { 3 } else { 2 };
            if i + count > fields.len() {
                return Err(error("malformed Git rename metadata"));
            }
            if rename {
                result.insert(path_field(fields[i + 1])?, path_field(fields[i + 2])?);
            }
            i += count;
        }
        Ok(result)
    }
    pub fn virtual_base(&self, left: &str, right: &str) -> io::Result<Option<String>> {
        self.virtual_base_at(left, right, 0)
    }
    fn virtual_base_at(&self, left: &str, right: &str, depth: usize) -> io::Result<Option<String>> {
        let (code, output) = self.status(&["merge-base", "--all", left, right])?;
        if code != 0 {
            return Ok(None);
        }
        let bases = words(&output)?;
        let Some(first) = bases.first() else {
            return Ok(None);
        };
        if bases.len() == 1 {
            return Ok(Some(first.clone()));
        }
        if depth >= 8 {
            return Ok(None);
        }
        let mut current = first.clone();
        for other in &bases[1..] {
            let inner = self.virtual_base_at(&current, other, depth + 1)?;
            let mut args = vec!["merge-tree".to_owned(), "--write-tree".into()];
            if let Some(inner) = inner {
                args.push(format!("--merge-base={inner}"));
            }
            args.extend([current.clone(), other.clone()]);
            let (code, output) =
                self.status(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
            if !matches!(code, 0 | 1) {
                return Ok(None);
            }
            let tree = String::from_utf8_lossy(&output)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_owned();
            if !oid(&tree) {
                return Ok(None);
            }
            let mut env = self.environment.clone();
            for role in ["AUTHOR", "COMMITTER"] {
                for (field, value) in [
                    ("NAME", "rsi landing guard"),
                    ("EMAIL", "landing-guard@invalid"),
                    ("DATE", "2000-01-01T00:00:00Z"),
                ] {
                    env.insert(format!("GIT_{role}_{field}").into(), value.into());
                }
            }
            let (code, output, _) = self.git_with(
                &[
                    "commit-tree".into(),
                    tree.into(),
                    "-m".into(),
                    "virtual merge base".into(),
                ],
                &env,
            )?;
            if code != 0 {
                return Ok(None);
            }
            current = String::from_utf8(output)
                .map_err(|_| error("non-UTF8 commit identity"))?
                .trim()
                .to_owned();
            if !oid(&current) {
                return Ok(None);
            }
        }
        Ok(Some(current))
    }
    pub fn own_base(&self, base: &str, source: &str, target: &str) -> io::Result<String> {
        if self.ancestor(source, target)? {
            return Ok(base.into());
        }
        let (code, output) = self.status(&["merge-base", "--all", source, target])?;
        if code != 0 {
            return Ok(base.into());
        }
        let bases = words(&output)?;
        let mut may_narrow = false;
        for shared in &bases {
            if self.ancestor(base, shared)? {
                may_narrow = true;
                break;
            }
        }
        if !may_narrow {
            return Ok(base.into());
        }
        let shared = if bases.len() == 1 {
            Some(bases[0].clone())
        } else {
            self.virtual_base(source, target)?
        };
        Ok(shared.unwrap_or_else(|| base.into()))
    }
    /// No migration masking: the plain accepted-source check.
    pub fn lost_hunks(&self, base: &str, source: &str, candidate: &str) -> io::Result<Vec<String>> {
        self.lost_hunks_proved(base, source, candidate, None)
    }
    /// The check with independently validated #361 proof sites applied to the
    /// accepted source (a proof only masks its exact, unambiguous version spans).
    pub fn lost_hunks_proved(
        &self,
        base: &str,
        source: &str,
        candidate: &str,
        proof: Option<&Proof>,
    ) -> io::Result<Vec<String>> {
        let renames = self.renamed_paths(source, candidate)?;
        let mut findings = Vec::new();
        for path in self.paths_changed(base, source)? {
            self.check()?;
            let candidate_path = proof
                .and_then(|proof| proof.destination(&path))
                .or_else(|| renames.get(&path))
                .unwrap_or(&path);
            let before = self.blob(base, &path)?;
            let mut after = self.blob(source, &path)?;
            let now = self.blob(candidate, candidate_path)?;
            if let Some(proof) = proof {
                if path == Path::new("tools/released-migrations.json") {
                    // A derived fingerprint catalog; the proof validator already
                    // checked source, target, transformed block and candidate.
                    after.clone_from(&now);
                } else if let Some(bytes) = after.take() {
                    after = Some(proof.apply(&path, bytes)?);
                }
            }
            findings.extend(blob_findings(
                &path,
                candidate_path,
                before.as_deref(),
                after.as_deref(),
                now.as_deref(),
                &|| self.check(),
            )?);
        }
        Ok(findings)
    }
    pub fn affected_crates(&self, target: &str, candidate: &str) -> io::Result<Vec<String>> {
        let mut packages = BTreeSet::new();
        for path in self.paths_changed(target, candidate)? {
            let parts: Vec<_> = path.components().collect();
            if parts.len() < 3 || parts[0].as_os_str() != "crates" {
                continue;
            }
            let manifest: PathBuf = parts[..2].iter().collect::<PathBuf>().join("Cargo.toml");
            let contents = self
                .blob(candidate, &manifest)?
                .or(self.blob(target, &manifest)?)
                .ok_or_else(|| error(format!("missing crate manifest: {}", manifest.display())))?;
            let contents =
                std::str::from_utf8(&contents).map_err(|_| error("crate manifest is not UTF-8"))?;
            let doc: toml::Value = toml::from_str(contents).map_err(|e| error(e.to_string()))?;
            let name = doc
                .get("package")
                .and_then(|v| v.get("name"))
                .and_then(toml::Value::as_str)
                .ok_or_else(|| error("crate manifest has no package name"))?;
            packages.insert(name.to_owned());
        }
        Ok(packages.into_iter().collect())
    }
}
fn words(bytes: &[u8]) -> io::Result<Vec<String>> {
    Ok(std::str::from_utf8(bytes)
        .map_err(|_| error("Git object identities are not UTF-8"))?
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}
fn path_field(bytes: &[u8]) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(
            std::str::from_utf8(bytes).map_err(|_| error("Git path is not UTF-8"))?,
        ))
    }
}
fn path_fields(bytes: &[u8]) -> io::Result<Vec<PathBuf>> {
    if !bytes.is_empty() && bytes.last() != Some(&0) {
        return Err(error("unterminated Git path metadata"));
    }
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(path_field)
        .collect()
}

/// SequenceMatcher(None, a, b, autojunk=False): longest contiguous block,
/// earliest a then earliest b tie breaking, recursively split and coalesce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Match {
    a: usize,
    b: usize,
    len: usize,
}
fn matching_blocks(
    a: &[&[u8]],
    b: &[&[u8]],
    check: &impl Fn() -> io::Result<()>,
) -> io::Result<Vec<Match>> {
    let mut positions: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for (j, line) in b.iter().enumerate() {
        if j % 1024 == 0 {
            check()?;
        }
        positions.entry(*line).or_default().push(j);
    }
    let mut queue = vec![(0, a.len(), 0, b.len())];
    let mut matches = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        check()?;
        let mut best = Match {
            a: alo,
            b: blo,
            len: 0,
        };
        let mut previous = HashMap::<usize, usize>::new();
        for (i, line) in a.iter().enumerate().take(ahi).skip(alo) {
            check()?;
            let mut current = HashMap::new();
            if let Some(js) = positions.get(line) {
                for &j in js {
                    if j % 1024 == 0 {
                        check()?;
                    }
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let count = j
                        .checked_sub(1)
                        .and_then(|j| previous.get(&j))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    current.insert(j, count);
                    if count > best.len {
                        best = Match {
                            a: i + 1 - count,
                            b: j + 1 - count,
                            len: count,
                        };
                    }
                }
            }
            previous = current;
        }
        if best.len > 0 {
            if alo < best.a && blo < best.b {
                queue.push((alo, best.a, blo, best.b));
            }
            if best.a + best.len < ahi && best.b + best.len < bhi {
                queue.push((best.a + best.len, ahi, best.b + best.len, bhi));
            }
            matches.push(best);
        }
    }
    matches.sort_by_key(|m| (m.a, m.b, m.len));
    let mut merged: Vec<Match> = Vec::new();
    for item in matches {
        if let Some(last) = merged
            .last_mut()
            .filter(|last| last.a + last.len == item.a && last.b + last.len == item.b)
        {
            last.len += item.len;
        } else {
            merged.push(item);
        }
    }
    merged.push(Match {
        a: a.len(),
        b: b.len(),
        len: 0,
    });
    Ok(merged)
}
#[derive(Clone, Copy)]
struct Opcode {
    a: usize,
    ae: usize,
    b: usize,
    be: usize,
    equal: bool,
}
fn opcodes(
    a: &[&[u8]],
    b: &[&[u8]],
    check: &impl Fn() -> io::Result<()>,
) -> io::Result<Vec<Opcode>> {
    let mut codes = Vec::new();
    let (mut i, mut j) = (0, 0);
    for m in matching_blocks(a, b, check)? {
        if i < m.a || j < m.b {
            codes.push(Opcode {
                a: i,
                ae: m.a,
                b: j,
                be: m.b,
                equal: false,
            });
        }
        if m.len > 0 {
            codes.push(Opcode {
                a: m.a,
                ae: m.a + m.len,
                b: m.b,
                be: m.b + m.len,
                equal: true,
            });
        }
        i = m.a + m.len;
        j = m.b + m.len;
    }
    Ok(codes)
}
fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' || bytes[i] == b'\n' {
            if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            result.push(&bytes[start..=i]);
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        result.push(&bytes[start..]);
    }
    result
}
fn line_text(mut line: &[u8]) -> String {
    while line.last().is_some_and(|b| matches!(b, b'\r' | b'\n')) {
        line = &line[..line.len() - 1];
    }
    let mut text = String::new();
    while !line.is_empty() {
        match std::str::from_utf8(line) {
            Ok(valid) => {
                text.push_str(valid);
                break;
            }
            Err(e) => {
                text.push_str(std::str::from_utf8(&line[..e.valid_up_to()]).unwrap_or(""));
                let count = e.error_len().unwrap_or(line.len() - e.valid_up_to());
                for byte in &line[e.valid_up_to()..e.valid_up_to() + count] {
                    text.push_str(&format!("\\x{byte:02x}"));
                }
                line = &line[e.valid_up_to() + count..];
            }
        }
    }
    crate::py::repr(&text)
}
fn missing_boundary(
    matches: &[Opcode],
    old_len: usize,
    candidate_len: usize,
    position: usize,
) -> Option<usize> {
    if old_len == 0 && candidate_len == 0 {
        return Some(0);
    }
    for m in matches {
        if m.a < position && position < m.ae {
            return Some(m.b + position - m.a);
        }
        if position == 0 && m.a == 0 && m.b == 0 {
            return Some(0);
        }
        if position == old_len && m.ae == old_len && m.b + m.ae - m.a == candidate_len {
            return Some(candidate_len);
        }
    }
    let left = matches
        .iter()
        .filter(|m| m.ae == position)
        .map(|m| m.b + m.ae - m.a)
        .next_back();
    let right = matches.iter().find(|m| m.a == position).map(|m| m.b);
    left.filter(|left| Some(*left) == right)
}
fn blob_findings(
    path: &Path,
    candidate_path: &Path,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    now: Option<&[u8]>,
    check: &impl Fn() -> io::Result<()>,
) -> io::Result<Vec<String>> {
    let mut errors = Vec::new();
    let at = candidate_path.display();
    let original = path.display();
    let Some(after) = after else {
        if let Some(now) = now.filter(|now| Some(*now) == before) {
            errors.push(format!(
                "{at}: accepted file deletion restored base blob sha256:{}",
                sha256(now)
            ));
        }
        return Ok(errors);
    };
    let Some(now) = now else {
        return Ok(errors);
    };
    if [after, before.unwrap_or_default(), now]
        .iter()
        .any(|bytes| bytes.contains(&0))
    {
        if let Some(before) = before.filter(|before| *before != after && *before == now) {
            errors.push(format!(
                "{at}: accepted binary edit reverted to base blob sha256:{}",
                sha256(before)
            ));
        }
        return Ok(errors);
    }
    let old = lines(before.unwrap_or_default());
    let new = lines(after);
    let candidate = lines(now);
    let matches: Vec<_> = opcodes(&old, &candidate, check)?
        .into_iter()
        .filter(|m| m.equal)
        .collect();
    let source_matches = matching_blocks(&new, &candidate, check)?;
    for change in opcodes(&old, &new, check)?.into_iter().filter(|m| !m.equal) {
        if change.a == change.ae {
            if source_matches.iter().any(|m| {
                m.a <= change.b
                    && change.be <= m.a + m.len
                    && (m.a < change.b
                        || m.a + m.len > change.be
                        || (change.b == 0 && change.be == new.len()))
            }) {
                continue;
            }
            if let Some(boundary) = missing_boundary(&matches, old.len(), candidate.len(), change.a)
            {
                errors.push(format!("{at}:{}: accepted insertion {} from {original}:{} is missing at its base boundary",boundary+1,line_text(new[change.b]),change.b+1));
            }
        } else {
            for m in &matches {
                if m.a <= change.a && change.ae <= m.ae {
                    let accepted = if change.b < change.be {
                        format!("; replaced by accepted {}", line_text(new[change.b]))
                    } else {
                        "; accepted deletion was lost".into()
                    };
                    errors.push(format!(
                        "{at}:{}: restored base line {} from {original}:{}{accepted}",
                        m.b + change.a - m.a + 1,
                        line_text(old[change.a]),
                        change.a + 1
                    ));
                    break;
                }
            }
        }
    }
    Ok(errors)
}

pub fn test_commands(packages: &[String], filters: &[String]) -> io::Result<Vec<Vec<String>>> {
    let mut selected: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut order = Vec::new();
    for item in filters {
        let Some((package, filter)) = item.split_once('=') else {
            return Err(error(format!(
                "invalid test filter (expected package=filter): {item}"
            )));
        };
        if package.is_empty() || filter.is_empty() || filter.starts_with('-') {
            return Err(error(format!(
                "invalid test filter (expected package=filter): {item}"
            )));
        }
        if !selected.contains_key(package) {
            order.push(package.to_owned());
        }
        selected
            .entry(package.into())
            .or_default()
            .push(filter.into());
    }
    let mut gated = packages.to_vec();
    for package in order {
        if !gated.contains(&package) {
            gated.push(package);
        }
    }
    let mut commands = Vec::new();
    for package in gated {
        for filter in selected
            .get(&package)
            .cloned()
            .unwrap_or_else(|| vec![String::new()])
        {
            let mut command = vec!["cargo".into(), "test".into(), "-p".into(), package.clone()];
            if let Some(bin) = filter.strip_prefix("bin:") {
                command.extend(["--bin".into(), bin.into()]);
            } else if let Some(test) = filter.strip_prefix("test:") {
                command.extend(["--test".into(), test.into()]);
            } else {
                command.push("--lib".into());
                if !filter.is_empty() {
                    command.push(filter);
                }
            }
            commands.push(command);
        }
    }
    Ok(commands)
}

#[cfg(test)]
#[path = "landing_guard_tests.rs"]
mod tests;

/// Exact accepted identities, before source-own delta narrowing.
pub struct Accepted<'a> {
    pub base: &'a str,
    pub source: &'a str,
    pub target: &'a str,
    pub candidate: &'a str,
}
impl Accepted<'_> {
    pub fn validate(&self, repo: &Repository<'_>) -> io::Result<()> {
        for id in [self.base, self.source, self.target, self.candidate] {
            repo.commit(id)?;
        }
        for (older, newer, message) in [
            (
                self.base,
                self.source,
                "accepted base is not an ancestor of source",
            ),
            (
                self.source,
                self.candidate,
                "accepted source is not an ancestor of candidate",
            ),
            (
                self.target,
                self.candidate,
                "target tip is not an ancestor of candidate",
            ),
        ] {
            if !repo.ancestor(older, newer)? {
                return Err(error(message));
            }
        }
        Ok(())
    }
}

/// Both operations are required; no production default silently skips a policy
/// check or labels an unexecuted test green. The full migration-proof adapter,
/// committed target guard and actual Cargo adapter are not staged here yet.
pub trait Backend {
    fn check_candidate(&mut self, repo: &Repository<'_>, candidate: &str) -> io::Result<()>;
    fn run_tests(&mut self, candidate: &str, commands: &[Vec<String>]) -> io::Result<()>;
    /// Recompute a provisional-migration proof (#361) bound to the accepted pair.
    /// The default refuses: a backend without the migration policy never masks.
    fn validate_proof(
        &mut self,
        _repo: &Repository<'_>,
        _pair: &Accepted<'_>,
        _proof: &Path,
    ) -> io::Result<ValidatedProof> {
        Err(error("provisional proof validation is unavailable"))
    }
}

/// Orchestration without a migration proof. Required tests still run on lost hunks.
pub fn evaluate(
    repo: &Repository<'_>,
    pair: &Accepted<'_>,
    filters: &[String],
    plan_only: bool,
    backend: &mut impl Backend,
) -> io::Result<(i32, serde_json::Value)> {
    evaluate_with(repo, pair, filters, plan_only, None, backend)
}

/// `evaluate` with an optional exact-source provisional-migration proof file.
/// Order matches the legacy guard: static policy, proof recomputation, lost
/// hunks (keyed to the accepted base when proved), report, then tests.
pub fn evaluate_with(
    repo: &Repository<'_>,
    pair: &Accepted<'_>,
    filters: &[String],
    plan_only: bool,
    proof_file: Option<&Path>,
    backend: &mut impl Backend,
) -> io::Result<(i32, serde_json::Value)> {
    pair.validate(repo)?;
    backend.check_candidate(repo, pair.candidate)?;
    let proof = proof_file
        .map(|file| backend.validate_proof(repo, pair, file))
        .transpose()?;
    let base = if proof.is_some() {
        pair.base.to_owned()
    } else {
        repo.own_base(pair.base, pair.source, pair.target)?
    };
    let missing = repo.lost_hunks_proved(
        &base,
        pair.source,
        pair.candidate,
        proof.as_ref().map(|validated| &validated.proof),
    )?;
    let packages = repo.affected_crates(pair.target, pair.candidate)?;
    let commands = test_commands(&packages, filters)?;
    let unchanged = repo.git(&["rev-parse", &format!("{}^{{tree}}", pair.target)], true)?
        == repo.git(
            &["rev-parse", &format!("{}^{{tree}}", pair.candidate)],
            true,
        )?;
    let mut report = serde_json::json!({"outcome":if unchanged && missing.is_empty() { "already_on_rolling" } else { "candidate" },
        "accepted_source":pair.source,"candidate":pair.candidate,"lost_hunks":missing,"affected_crates":packages,
        "test_commands":commands.iter().map(|c|c.join(" ")).collect::<Vec<_>>(),"tests_executed":false});
    if let Some(validated) = &proof {
        report["provisional_migration"] = serde_json::json!({
            "assigned_version": validated.assigned_version,
            "unit_candidate": validated.unit_candidate,
            "proof_sha256": validated.digest,
        });
    }
    if !plan_only {
        backend.run_tests(pair.candidate, &commands)?;
        report["tests_executed"] = true.into();
    }
    Ok((i32::from(!missing.is_empty()), report))
}

/// Source spans of an independently re-derived #361 provisional-migration proof.
/// The JSON claim itself grants nothing: the caller recomputes it first.
pub struct Proof {
    sites: Vec<ProofSite>,
    destinations: BTreeMap<PathBuf, PathBuf>,
}
struct ProofSite {
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
}
impl Proof {
    pub fn from_value(proof: &serde_json::Value) -> io::Result<Self> {
        let list = proof
            .get("sites")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| error("provisional proof has no sites list"))?;
        let field = |site: &serde_json::Value, name: &str| -> io::Result<String> {
            site.get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| error(format!("provisional proof site has no {name}")))
        };
        let (mut sites, mut destinations) = (Vec::new(), BTreeMap::new());
        for site in list {
            let path = PathBuf::from(field(site, "path")?);
            let target = PathBuf::from(field(site, "target_path")?);
            if destinations
                .entry(path.clone())
                .or_insert_with(|| target.clone())
                != &target
            {
                return Err(error("one source file has conflicting proved destinations"));
            }
            sites.push(ProofSite {
                path,
                before: field(site, "before")?.into_bytes(),
                after: field(site, "after")?.into_bytes(),
            });
        }
        Ok(Self {
            sites,
            destinations,
        })
    }
    fn destination(&self, path: &Path) -> Option<&PathBuf> {
        self.destinations.get(path)
    }
    /// Replace each proved span exactly once; an absent or repeated span is refused.
    fn apply(&self, path: &Path, mut bytes: Vec<u8>) -> io::Result<Vec<u8>> {
        for site in self.sites.iter().filter(|site| site.path == path) {
            let mut at = None;
            let mut count = 0;
            let mut from = 0;
            while let Some(found) = find(&bytes[from..], &site.before) {
                count += 1;
                at.get_or_insert(from + found);
                from += found + site.before.len().max(1);
            }
            let Some(start) = at.filter(|_| count == 1) else {
                return Err(error(format!(
                    "proved source site is ambiguous: {}",
                    path.display()
                )));
            };
            bytes.splice(start..start + site.before.len(), site.after.iter().copied());
        }
        Ok(bytes)
    }
}
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A proof that the backend recomputed from the accepted pair; `digest` is the
/// `sha256:` identity of the proof file's exact bytes.
pub struct ValidatedProof {
    pub proof: Proof,
    pub assigned_version: serde_json::Value,
    pub unit_candidate: serde_json::Value,
    pub digest: String,
}
