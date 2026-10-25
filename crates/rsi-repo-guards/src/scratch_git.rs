//! Fixture Git safety shared by desktop, QA and landing guard callers (#363).
//!
//! Construct a policy from the permanent checkout, physical caller cwd and an
//! explicit environment. Validation is repeated at command preparation, never
//! inferred from a failed `cd`. No process environment or working directory is
//! mutated. Legacy Python is source-only while canonical callers converge.
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub type Environment = BTreeMap<OsString, OsString>;
pub const LOCATION_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

fn refusal(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Resolve physical components, including symlinks before `..`, while allowing
/// nonexistent tails like pathlib.resolve(strict=False). Never lossy-decode paths.
fn resolve(path: &Path, cwd: &Path) -> io::Result<PathBuf> {
    fn walk(path: &Path, mut result: PathBuf, links: &mut usize) -> io::Result<PathBuf> {
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => result.push(component.as_os_str()),
                Component::CurDir => (),
                Component::ParentDir => {
                    result.pop();
                }
                Component::Normal(name) => {
                    result.push(name);
                    match fs::symlink_metadata(&result) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            *links += 1;
                            if *links > 40 {
                                return Err(refusal("too many symbolic links in scratch path"));
                            }
                            let target = fs::read_link(&result)?;
                            result.pop();
                            result = walk(
                                &target,
                                if target.is_absolute() {
                                    PathBuf::new()
                                } else {
                                    result
                                },
                                links,
                            )?;
                        }
                        Ok(_) => (),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(result)
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let resolved = walk(&absolute, PathBuf::new(), &mut 0)?;
    // On Windows canonicalize uses the verbatim drive/UNC spelling. Match the
    // physical cwd and validated targets, including nonexistent override tails.
    #[cfg(windows)]
    {
        let mut ancestor = resolved.as_path();
        let mut tail = Vec::new();
        loop {
            match fs::canonicalize(ancestor) {
                Ok(mut physical) => {
                    for name in tail.iter().rev() {
                        physical.push(name);
                    }
                    return Ok(physical);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let Some(name) = ancestor.file_name() else {
                        return Err(error);
                    };
                    tail.push(name.to_owned());
                    ancestor = ancestor.parent().ok_or(error)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
    #[cfg(not(windows))]
    Ok(resolved)
}

fn lexical(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

/// Git skips relative entries. An empty entry switches subsequent absolute
/// entries to lexical normalization; it does not resolve their symlinks (#2634).
pub fn ceiling_directories(environment: &Environment, cwd: &Path) -> Vec<PathBuf> {
    let Some(value) = environment.get(OsStr::new("GIT_CEILING_DIRECTORIES")) else {
        return vec![];
    };
    let mut physical = true;
    let mut ceilings = Vec::new();
    for entry in std::env::split_paths(value) {
        if entry.as_os_str().is_empty() {
            physical = false;
        } else if entry.is_absolute() {
            if physical {
                if let Ok(path) = resolve(&entry, cwd) {
                    ceilings.push(path);
                }
            } else {
                ceilings.push(lexical(&entry));
            }
        }
    }
    ceilings
}

fn worktree_root(start: &Path, ceilings: &[PathBuf]) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate != start && ceilings.iter().any(|c| c == candidate) {
            return None;
        }
        if candidate.join(".git").exists() {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn gitdir_of(root: &Path) -> io::Result<Option<PathBuf>> {
    let entry = root.join(".git");
    if entry.is_dir() {
        return fs::canonicalize(entry).map(Some);
    }
    let bytes = match fs::read(&entry) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(target) = bytes.strip_prefix(b"gitdir:") else {
        return Ok(None);
    };
    let start = target
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(target.len());
    let end = target
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    #[cfg(unix)]
    let target = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(&target[start..end]))
    };
    #[cfg(not(unix))]
    let target = PathBuf::from(
        std::str::from_utf8(&target[start..end]).map_err(|_| refusal("non-UTF8 gitdir pointer"))?,
    );
    resolve(&target, root).map(Some)
}

/// Caller-owned context. The permanent checkout is mandatory and remains
/// protected even when a ceiling hides the repository around the caller cwd.
#[derive(Clone)]
pub struct Policy {
    checkout: PathBuf,
    cwd: PathBuf,
    environment: Environment,
    extra: Vec<PathBuf>,
}

impl Policy {
    pub fn new(
        checkout: &Path,
        cwd: &Path,
        environment: Environment,
        extra: &[PathBuf],
    ) -> io::Result<Self> {
        if !checkout.is_absolute() || !cwd.is_absolute() {
            return Err(refusal("checkout and caller cwd must be absolute"));
        }
        let checkout = fs::canonicalize(checkout)?;
        let cwd = fs::canonicalize(cwd)?;
        if !checkout.is_dir() || !cwd.is_dir() {
            return Err(refusal("checkout and caller cwd must be directories"));
        }
        Ok(Self {
            checkout,
            cwd,
            environment,
            extra: extra.to_vec(),
        })
    }

    pub fn from_process(checkout: &Path) -> io::Result<Self> {
        Self::new(
            checkout,
            &std::env::current_dir()?,
            std::env::vars_os().collect(),
            &[],
        )
    }

    pub fn protected_roots(&self) -> io::Result<Vec<PathBuf>> {
        let mut roots = vec![self.checkout.clone()];
        if let Some(root) = worktree_root(
            &self.cwd,
            &ceiling_directories(&self.environment, &self.cwd),
        ) {
            roots.push(root);
        }
        for root in roots.clone() {
            if let Some(gitdir) = gitdir_of(&root)? {
                roots.push(gitdir);
            }
        }
        for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
            if let Some(value) = self
                .environment
                .get(OsStr::new(name))
                .filter(|v| !v.is_empty())
            {
                roots.push(resolve(Path::new(value), &self.cwd)?);
            }
        }
        for path in &self.extra {
            roots.push(resolve(path, &self.cwd)?);
        }
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    pub fn validate(&self, directory: &Path) -> io::Result<PathBuf> {
        if directory.as_os_str().is_empty() {
            return Err(refusal("scratch directory is empty"));
        }
        if !directory.is_absolute() {
            return Err(refusal(format!(
                "scratch directory is not absolute: {directory:?}"
            )));
        }
        let resolved = fs::canonicalize(directory).map_err(|e| {
            refusal(format!(
                "scratch directory does not exist: {directory:?} ({e})"
            ))
        })?;
        if !resolved.is_dir() {
            return Err(refusal(format!(
                "scratch path is not a directory: {directory:?}"
            )));
        }
        if resolved.parent().is_none() {
            return Err(refusal("scratch directory is the filesystem root"));
        }
        if let Some(root) = self
            .protected_roots()?
            .iter()
            .find(|root| resolved.starts_with(root))
        {
            return Err(refusal(format!(
                "scratch directory {directory:?} resolves to {resolved:?}, inside protected root {root:?}; Git there would act on the assigned sandbox"
            )));
        }
        Ok(resolved)
    }

    /// Prepare only after validation. Callers retain their own process deadline,
    /// cancellation and output contract; this API does not start a supervisor.
    pub fn command<S: AsRef<OsStr>>(&self, directory: &Path, args: &[S]) -> io::Result<Command> {
        reject_location_overrides(args)?;
        let resolved = self.validate(directory)?;
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&resolved)
            .args(args)
            .current_dir(&resolved)
            .env_clear()
            .envs(clean_environment(&resolved, &self.environment)?);
        Ok(command)
    }

    /// The first actually writable host temp parent outside every protected root.
    /// Candidates are injectable for fail-closed and job-ceiling tests.
    pub fn host_temp_root_from(&self, candidates: &[PathBuf]) -> io::Result<PathBuf> {
        let roots = self.protected_roots()?;
        for candidate in candidates {
            let Ok(candidate) = fs::canonicalize(candidate) else {
                continue;
            };
            if !candidate.is_dir() || roots.iter().any(|root| candidate.starts_with(root)) {
                continue;
            }
            // A real allocation checks ACLs as well as mode bits, on every host.
            if tempfile::Builder::new()
                .prefix("rsi-scratch-probe-")
                .tempdir_in(&candidate)
                .is_ok()
            {
                return Ok(candidate);
            }
        }
        Err(refusal(
            "no temporary directory outside the protected roots is available",
        ))
    }

    pub fn host_temp_root(&self) -> io::Result<PathBuf> {
        let mut candidates: Vec<PathBuf> = ["TMPDIR", "TEMP", "TMP"]
            .iter()
            .filter_map(|key| {
                self.environment
                    .get(OsStr::new(key))
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
            .collect();
        candidates.push(std::env::temp_dir());
        candidates.extend(["/var/tmp", "/private/tmp", "/tmp", "/dev/shm"].map(PathBuf::from));
        self.host_temp_root_from(&candidates)
    }

    pub fn tempdir_in(&self, prefix: &str, parent: &Path) -> io::Result<tempfile::TempDir> {
        if prefix.contains(['/', '\\']) {
            return Err(refusal("scratch prefix must be a filename prefix"));
        }
        let parent = self.host_temp_root_from(&[parent.to_owned()])?;
        let temp = tempfile::Builder::new().prefix(prefix).tempdir_in(parent)?;
        self.validate(temp.path())?;
        Ok(temp)
    }

    pub fn tempdir(&self, prefix: &str) -> io::Result<tempfile::TempDir> {
        self.tempdir_in(prefix, &self.host_temp_root()?)
    }
}

pub fn clean_environment(resolved: &Path, base: &Environment) -> io::Result<Environment> {
    let mut environment = base.clone();
    for key in LOCATION_VARIABLES {
        environment.remove(OsStr::new(key));
    }
    let parent = resolved
        .parent()
        .ok_or_else(|| refusal("scratch directory is the filesystem root"))?;
    // join_paths preserves native paths and refuses an unrepresentable ceiling.
    let ceiling = std::env::join_paths([parent]).map_err(|e| refusal(e.to_string()))?;
    environment.insert("GIT_CEILING_DIRECTORIES".into(), ceiling);
    Ok(environment)
}

pub fn reject_location_overrides<S: AsRef<OsStr>>(args: &[S]) -> io::Result<()> {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        // Options are ASCII; do not replace invalid filename bytes with U+FFFD.
        let text = arg.as_ref().to_string_lossy();
        if ["--git-dir", "--work-tree", "--namespace", "--super-prefix"]
            .iter()
            .any(|option| text == *option || text.starts_with(&format!("{option}=")))
            || text.starts_with("-C")
        {
            return Err(refusal(format!(
                "git option {:?} would override the scratch directory",
                arg.as_ref()
            )));
        }
        if text == "-c" || text == "--config-env" {
            index += 2;
        } else if text.starts_with('-') {
            index += 1;
        } else {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "scratch_git_tests.rs"]
mod tests;
