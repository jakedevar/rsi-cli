//! Artifact manifests and safe handling of collected evidence (#3683 R5, design #3628 parity row
//! on logs and artifacts: "Hub keeps ... verifier directory, raw trace and trajectory, plus
//! artifact manifest. Copy before removing remote resources. Collection failure is explicit").
//!
//! What a trial container leaves under `/logs` is **untrusted**: a model run can plant symlinks
//! that point at hub files, special files, absurdly deep trees or huge files. [`build_manifest`]
//! therefore never follows a symlink, reads only regular files, hashes them in streaming fashion
//! and records everything it refused with a reason. Its limits are hard: when one is hit the
//! manifest says `truncated` instead of walking on. [`verify_manifest`] later proves the
//! evidence is still the bytes that were listed.

use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hard bounds on what is walked and hashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_files: usize,
    pub max_depth: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_depth: 32,
            max_file_bytes: 256 * (1 << 20),
            max_total_bytes: 2 * (1 << 30),
        }
    }
}

/// One regular file of the evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactEntry {
    /// Forward-slash path relative to the manifest root.
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// An entry that was seen and deliberately not read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Never followed: it can point anywhere on the hub.
    Symlink,
    /// A socket, FIFO or device.
    SpecialFile,
    /// A name that is not UTF-8, holds a control character or a backslash.
    UnsafeName,
    OversizeFile,
    /// Past `max_depth`.
    TooDeep,
    /// The file changed while it was being read.
    Unstable,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ArtifactManifest {
    pub files: Vec<ArtifactEntry>,
    pub skipped: Vec<SkippedEntry>,
    pub total_bytes: u64,
    /// A limit was hit: the manifest is a prefix of the evidence, not all of it.
    pub truncated: bool,
}

/// A relative path an archive or manifest may name: non-empty, relative, no `..`, no empty or
/// `.` component, no backslash, no NUL or other control character.
#[must_use]
pub fn is_safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
}

fn hash_file(path: &Path, expected: u64) -> std::io::Result<Option<(u64, String)>> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > expected {
            // It grew while we read it.
            return Ok(None);
        }
        hasher.update(&buffer[..n]);
    }
    if total != expected {
        return Ok(None);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(Some((total, hex)))
}

struct Walk {
    limits: Limits,
    out: ArtifactManifest,
}

impl Walk {
    fn skip(&mut self, rel: &str, reason: SkipReason) {
        // A bad name is recorded with a lossy, printable spelling.
        self.out.skipped.push(SkippedEntry {
            path: rel.to_string(),
            reason,
        });
    }

    fn visit(&mut self, dir: &Path, rel: &str, depth: usize) {
        let Ok(read) = fs::read_dir(dir) else {
            self.skip(
                if rel.is_empty() { "." } else { rel },
                SkipReason::Unreadable,
            );
            return;
        };
        let mut entries: Vec<(std::ffi::OsString, PathBuf)> = read
            .filter_map(Result::ok)
            .map(|e| (e.file_name(), e.path()))
            .collect();
        entries.sort();
        for (name, path) in entries {
            if self.out.truncated {
                return;
            }
            let lossy = name.to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                lossy.clone()
            } else {
                format!("{rel}/{lossy}")
            };
            let Some(text) = name.to_str().filter(|n| safe_name(n)) else {
                self.skip(&child, SkipReason::UnsafeName);
                continue;
            };
            let child = if rel.is_empty() {
                text.to_string()
            } else {
                format!("{rel}/{text}")
            };
            let Ok(meta) = fs::symlink_metadata(&path) else {
                self.skip(&child, SkipReason::Unreadable);
                continue;
            };
            let kind = meta.file_type();
            if kind.is_symlink() {
                self.skip(&child, SkipReason::Symlink);
            } else if kind.is_dir() {
                if depth + 1 > self.limits.max_depth {
                    self.skip(&child, SkipReason::TooDeep);
                } else {
                    self.visit(&path, &child, depth + 1);
                }
            } else if kind.is_file() {
                self.file(&path, &child, meta.len());
            } else {
                self.skip(&child, SkipReason::SpecialFile);
            }
        }
    }

    fn file(&mut self, path: &Path, rel: &str, len: u64) {
        if self.out.files.len() >= self.limits.max_files
            || self.out.total_bytes.saturating_add(len) > self.limits.max_total_bytes
        {
            self.out.truncated = true;
            return;
        }
        if len > self.limits.max_file_bytes {
            self.skip(rel, SkipReason::OversizeFile);
            return;
        }
        match hash_file(path, len) {
            Ok(Some((bytes, sha256))) => {
                self.out.total_bytes += bytes;
                self.out.files.push(ArtifactEntry {
                    path: rel.to_string(),
                    bytes,
                    sha256,
                });
            }
            Ok(None) => self.skip(rel, SkipReason::Unstable),
            Err(_) => self.skip(rel, SkipReason::Unreadable),
        }
    }
}

/// Walk `root` without following any link and list what is there.
///
/// # Errors
/// The root itself is missing, is not a plain directory, or is a symlink.
pub fn build_manifest(root: &Path, limits: Limits) -> Result<ArtifactManifest, String> {
    let meta = fs::symlink_metadata(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if !meta.file_type().is_dir() {
        return Err(format!(
            "{} is not a plain directory (a symlink or file is never walked)",
            root.display()
        ));
    }
    let mut walk = Walk {
        limits,
        out: ArtifactManifest::default(),
    };
    walk.visit(root, "", 0);
    walk.out.files.sort_by(|a, b| a.path.cmp(&b.path));
    walk.out.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(walk.out)
}

/// Prove the evidence is still what the manifest lists: every file present, a regular file (not
/// a link), the same size and digest. Returns one line per difference; empty means intact.
#[must_use]
pub fn verify_manifest(root: &Path, manifest: &ArtifactManifest) -> Vec<String> {
    let mut problems = Vec::new();
    for entry in &manifest.files {
        if !is_safe_relative(&entry.path) {
            problems.push(format!("{}: unsafe path in the manifest", entry.path));
            continue;
        }
        let path = root.join(&entry.path);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => match hash_file(&path, meta.len()) {
                Ok(Some((bytes, sha))) if bytes == entry.bytes && sha == entry.sha256 => {}
                _ => problems.push(format!("{}: changed since the manifest", entry.path)),
            },
            Ok(_) => problems.push(format!("{}: no longer a regular file", entry.path)),
            Err(_) => problems.push(format!("{}: missing", entry.path)),
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_rules() {
        for ok in ["a", "a/b.txt", "logs/agent/oracle.txt"] {
            assert!(is_safe_relative(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a\\b",
            "a\nb",
            "a/",
        ] {
            assert!(!is_safe_relative(bad), "{bad:?}");
        }
    }
}
