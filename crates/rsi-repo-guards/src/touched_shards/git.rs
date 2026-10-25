//! Read pinned Git objects; planning never resolves Cargo or executes a helper.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub type Texts = BTreeMap<String, String>;

#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
}

impl Repository {
    pub fn new(root: &Path) -> io::Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
        })
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root).env("GIT_OPTIONAL_LOCKS", "0");
        cmd
    }

    pub fn run(&self, args: &[&str]) -> io::Result<Output> {
        self.command().args(args).output()
    }

    pub fn text(&self, args: &[&str]) -> io::Result<String> {
        let result = self.run(args)?;
        if !result.status.success() {
            return Err(io::Error::other(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&result.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&result.stdout).into_owned())
    }

    pub fn revision(&self, revision: &str) -> io::Result<String> {
        self.text(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ])
        .map(|s| s.trim().into())
    }

    pub fn files(&self, revision: &str) -> io::Result<Vec<String>> {
        let result = self.run(&["ls-tree", "-rz", "--name-only", revision])?;
        if !result.status.success() {
            return Err(io::Error::other("git ls-tree failed"));
        }
        result
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8(s.to_vec()).map_err(io::Error::other))
            .collect()
    }

    /// One batch, with stdin and stdout drained concurrently (large trees can
    /// fill both pipe buffers). Missing objects are errors, never empty sources.
    pub fn blobs(&self, revision: &str, paths: &[String]) -> io::Result<Texts> {
        if paths.is_empty() {
            return Ok(Texts::new());
        }
        if paths.iter().any(|p| p.contains(['\n', '\r'])) {
            return Err(io::Error::other("Git batch paths cannot contain newlines"));
        }
        let request = paths
            .iter()
            .map(|p| format!("{revision}:{p}\n"))
            .collect::<String>();
        let mut child = self
            .command()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("git stdin absent"))?;
        let writer = std::thread::spawn(move || input.write_all(request.as_bytes()));
        let output = child.wait_with_output();
        let sent = writer
            .join()
            .map_err(|_| io::Error::other("git input writer panicked"))?;
        let output = output?;
        sent?;
        if !output.status.success() {
            return Err(io::Error::other("git cat-file failed"));
        }
        decode_blobs(paths, &output.stdout)
    }

    pub fn diff(&self, base: &str, head: &str, path: &str) -> io::Result<String> {
        self.text(&[
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            base,
            head,
            "--",
            path,
        ])
    }
}

fn decode_blobs(paths: &[String], bytes: &[u8]) -> io::Result<Texts> {
    let mut cursor = 0;
    let mut texts = Texts::new();
    for path in paths {
        let newline = bytes[cursor..]
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| io::Error::other("truncated git object header"))?
            + cursor;
        let header = String::from_utf8_lossy(&bytes[cursor..newline]);
        let words: Vec<_> = header.split_whitespace().collect();
        if words.len() != 3 || words[1] != "blob" {
            return Err(io::Error::other(format!(
                "expected Git blob: {path}: {header}"
            )));
        }
        let size: usize = words[2].parse().map_err(io::Error::other)?;
        cursor = newline + 1;
        let end = cursor
            .checked_add(size)
            .ok_or_else(|| io::Error::other("Git object size overflow"))?;
        if bytes.get(end) != Some(&b'\n') {
            return Err(io::Error::other("truncated git object body"));
        }
        texts.insert(
            path.clone(),
            String::from_utf8_lossy(&bytes[cursor..end]).into_owned(),
        );
        cursor = end + 1;
    }
    Ok(texts)
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DiffContext {
    pub base: String,
    pub head: String,
    pub merge_base: String,
    pub audit_base: String,
    pub merge_bases: Vec<String>,
    pub merged_tree: Option<String>,
    pub merge_clean: Option<bool>,
    pub changed: Vec<String>,
    pub added: Vec<String>,
}

fn names(repo: &Repository, from: &str, to: &str, added: bool) -> io::Result<BTreeSet<String>> {
    let mut args = vec!["diff", "--name-only", "-z"];
    if added {
        args.push("--diff-filter=A");
    }
    args.extend([from, to, "--"]);
    let result = repo.run(&args)?;
    if !result.status.success() {
        return Err(io::Error::other("git diff --name-only failed"));
    }
    result
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8(s.to_vec()).map_err(io::Error::other))
        .collect()
}

impl DiffContext {
    /// Preserve #1633/#2814: with multiple merge bases the integrated tree's
    /// delta from the base tip owns the candidate paths. Conflict fallback is
    /// the tip delta intersected with the union of merge-base deltas.
    pub fn load(repo: &Repository, base: &str, head: &str) -> io::Result<Self> {
        let base = repo.revision(base)?;
        let head = repo.revision(head)?;
        let mb = repo.run(&["merge-base", "--all", &base, &head])?;
        let merge_bases: Vec<String> = if mb.status.success() {
            String::from_utf8_lossy(&mb.stdout)
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        } else if mb.status.code() == Some(1) {
            Vec::new()
        } else {
            return Err(io::Error::other("git merge-base failed"));
        };
        let multiple = merge_bases.len() > 1;
        let merge_base = if multiple {
            base.clone()
        } else {
            merge_bases.first().unwrap_or(&base).clone()
        };
        let audit_base = merge_bases.first().unwrap_or(&base).clone();
        let merged = repo.run(&["merge-tree", "--write-tree", &base, &head])?;
        let merge_clean = match merged.status.code() {
            Some(0) => Some(true),
            Some(1) => Some(false),
            _ => None,
        };
        let merged_tree = if multiple && merged.status.success() {
            String::from_utf8_lossy(&merged.stdout)
                .split_whitespace()
                .next()
                .map(str::to_owned)
        } else {
            None
        };
        let diff_names = |added| -> io::Result<Vec<String>> {
            let result = if !multiple {
                names(repo, &merge_base, &head, added)?
            } else if let Some(tree) = &merged_tree {
                names(repo, &base, tree, added)?
            } else {
                let tip = names(repo, &base, &head, added)?;
                let mut seen = BTreeSet::new();
                for each in &merge_bases {
                    seen.extend(names(repo, each, &head, added)?);
                }
                tip.intersection(&seen).cloned().collect()
            };
            Ok(result.into_iter().collect())
        };
        let changed = diff_names(false)?;
        let added = diff_names(true)?;
        Ok(Self {
            base,
            head,
            merge_base,
            audit_base,
            merge_bases,
            merged_tree,
            merge_clean,
            changed,
            added,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_first: usize,
    pub old_count: usize,
    pub new_first: usize,
    pub new_count: usize,
}

pub fn changed_line_ranges(diff: &str) -> Vec<Hunk> {
    fn range(text: &str) -> Option<(usize, usize)> {
        let text = text.get(1..)?;
        let (first, count) = text.split_once(',').unwrap_or((text, "1"));
        Some((first.parse().ok()?, count.parse().ok()?))
    }
    diff.lines()
        .filter_map(|row| {
            let mut words = row.strip_prefix("@@ ")?.split_whitespace();
            let (old_first, old_count) = range(words.next()?)?;
            let (new_first, new_count) = range(words.next()?)?;
            if words.next()? != "@@" {
                return None;
            }
            Some(Hunk {
                old_first,
                old_count,
                new_first,
                new_count,
            })
        })
        .collect()
}

impl Repository {
    /// Paths of every `.rs` file that differs between two revisions, NUL-delimited so
    /// that spaces, tabs, quotes and non-ASCII names arrive byte-exact (never C-quoted).
    pub fn rust_diff_names(&self, from: &str, to: &str) -> io::Result<BTreeSet<String>> {
        let result = self.run(&[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            from,
            to,
            "--",
            "*.rs",
        ])?;
        if !result.status.success() {
            return Err(io::Error::other("git diff --name-only failed"));
        }
        result
            .stdout
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8(s.to_vec()).map_err(io::Error::other))
            .collect()
    }

    /// The `-U0` diff of exactly one file. The path travels as a literal pathspec
    /// argument, so no header (`+++ b/...`) has to be parsed back into a path.
    pub fn file_diff(&self, from: &str, to: &str, path: &str) -> io::Result<String> {
        let spec = format!(":(literal){path}");
        self.text(&[
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            from,
            to,
            "--",
            &spec,
        ])
    }
}

/// The added lines of one file diff as `(new line number, text)`, read from the hunk
/// bodies only (the header block ends at the first `@@`).
pub fn added_lines_of(diff: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut line = 0usize;
    let mut in_hunk = false;
    for raw in diff.lines() {
        if let Some(rest) = raw.strip_prefix("@@") {
            in_hunk = true;
            line = rest
                .split_whitespace()
                .find_map(|word| word.strip_prefix('+'))
                .and_then(|n| n.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if in_hunk {
            if let Some(text) = raw.strip_prefix('+') {
                out.push((line, text.to_owned()));
                line += 1;
            }
        }
    }
    out
}
