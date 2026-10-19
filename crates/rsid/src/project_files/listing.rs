//! Listings use no-follow directory handles for names and metadata. Git supplies
//! index/ignore policy only; it never walks the working tree for these RPCs.

use super::*;
use cap_std::fs::Dir;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rsi_common::asset_fs::AssetDir;
use std::ffi::OsString;

fn io_refusal(error: std::io::Error) -> DaemonError {
    let code = match error.kind() {
        std::io::ErrorKind::Unsupported => write::UNSUPPORTED,
        std::io::ErrorKind::NotFound => NOT_FOUND,
        _ => write::IO_FAILED,
    };
    refuse(code, error)
}

pub(super) struct DirectoryScope {
    root_path: PathBuf,
    root: AssetDir,
    pub(super) scoped: ScopedPath,
}

impl DirectoryScope {
    pub(super) fn resolve(root: &Path, requested: &str) -> Result<Self> {
        // Pin before canonicalization, which preserves contained symlink aliases.
        let pinned = AssetDir::open(root).map_err(io_refusal)?;
        let scoped = resolve_in_root(root, requested)?;
        #[cfg(test)]
        tests::after_resolve();
        let scope = Self {
            root_path: root.to_owned(),
            root: pinned,
            scoped,
        };
        scope.open()?;
        Ok(scope)
    }

    fn validate(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let now = AssetDir::open(&self.root_path).map_err(io_refusal)?;
            let before = self.root.try_clone_file()?.metadata()?;
            let after = now.try_clone_file()?.metadata()?;
            if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
                return Err(refuse(write::IO_FAILED, "project root was replaced"));
            }
        }
        Ok(())
    }

    fn open(&self) -> Result<AssetDir> {
        self.validate()?;
        let relative = self
            .scoped
            .target
            .strip_prefix(&self.root_path)
            .map_err(|_| refuse(OUTSIDE_ROOT, "directory escaped its pinned root"))?;
        self.root.open_dir(relative).map_err(|error| {
            // Keep the existing regular-file-as-directory refusal. A swapped
            // symlink also fails this open; neither its names nor metadata leak.
            if error.kind() == std::io::ErrorKind::NotADirectory {
                refuse(NOT_A_DIRECTORY, &self.scoped.display)
            } else {
                io_refusal(error)
            }
        })
    }

    pub(super) fn entries(&self, limit: usize) -> Result<(Vec<ProjectDirectoryEntryV1>, bool)> {
        let directory = self.open()?;
        let names = directory.entries(limit).map_err(io_refusal)?;
        let truncated = names.len() > limit;
        let metadata_dir = Dir::from_std_file(directory.try_clone_file()?);
        let mut entries = Vec::new();
        for name in names.into_iter().take(limit) {
            #[cfg(test)]
            tests::before_metadata();
            // Handle-relative lstat: no ancestor reopen, no final link follow.
            let Ok(metadata) = metadata_dir.symlink_metadata(&name) else {
                continue;
            };
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                ProjectEntryKindV1::Symlink
            } else if file_type.is_dir() {
                ProjectEntryKindV1::Dir
            } else {
                ProjectEntryKindV1::File
            };
            let name = name.to_string_lossy().into_owned();
            entries.push(ProjectDirectoryEntryV1 {
                path: join_display(&self.scoped.display, &name),
                hidden: name.starts_with('.'),
                size: metadata.len(),
                mtime: metadata
                    .modified()
                    .ok()
                    .map(|time| rfc3339(time.into_std())),
                name,
                kind,
                ignored: false,
            });
        }
        // Refuse swaps even after the target handle was opened.
        self.open()?;
        Ok((entries, truncated))
    }

    pub(super) async fn git(
        &self,
        args: &[&str],
        stdin: Option<Vec<u8>>,
    ) -> Result<Option<GitOutput>> {
        self.open()?;
        let output = run_git(
            &self.root_path,
            args,
            stdin,
            Some(self.root.try_clone_file()?),
        )
        .await;
        self.open()?;
        Ok(output)
    }

    async fn inside_work_tree(&self) -> Result<bool> {
        let Some(output) = self.git(&["rev-parse", "--show-toplevel"], None).await? else {
            return Ok(false);
        };
        if output.code != Some(0) {
            return Ok(false);
        }
        let top = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if Path::new(&top) == self.root_path {
            return Ok(true);
        }
        // A scratch root ignored by its enclosing repo keeps using the walk.
        let ignored = self
            .git(&["check-ignore", "-q", "--", "."], None)
            .await?
            .is_some_and(|output| output.code == Some(0));
        Ok(!ignored)
    }
}

pub(super) async fn list_files(
    root: PathBuf,
    requested: String,
    max_paths: Option<usize>,
) -> Result<ListProjectFilesResponseV1> {
    #[cfg(test)]
    let hook = take_resolve_hook();
    let scope = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        install_resolve_hook(hook);
        DirectoryScope::resolve(&root, &requested)
    })
    .await
    .map_err(|e| DaemonError::Process(e.to_string()))??;
    list_files_scoped(scope, max_paths).await
}

async fn list_files_scoped(
    scope: DirectoryScope,
    max_paths: Option<usize>,
) -> Result<ListProjectFilesResponseV1> {
    let cap = clamp(max_paths, DEFAULT_PICKER_PATHS, MAX_PICKER_PATHS);
    let mut tracked = None;
    let mut gitlinks = HashSet::new();
    if scope.inside_work_tree().await? {
        let pathspec = format!(":(literal){}", scope.scoped.real_relative);
        let mut args = vec!["ls-files", "-z", "--cached", "--stage"];
        if !scope.scoped.real_relative.is_empty() {
            args.extend(["--", &pathspec]);
        }
        if let Some(output) = scope.git(&args, None).await?
            && output.code == Some(0)
            && !output.truncated
        {
            let mut paths = Vec::new();
            for record in nul_fields(&output.stdout) {
                let Some((stage, path)) = record.split_once('\t') else {
                    continue;
                };
                if stage.starts_with("160000 ") {
                    gitlinks.insert(path.to_owned());
                }
                paths.push(path.to_owned());
            }
            tracked = Some(paths);
        }
    }
    let git_mode = tracked.is_some();
    let (scope, mut paths, mut truncated) = tokio::task::spawn_blocking(move || {
        let mut paths = Vec::new();
        let mut truncated = false;
        let mut rules = IgnoreRules::parents(&scope)?;
        rules.gitlinks = gitlinks;
        walk(
            &scope.open()?,
            &scope.scoped.target,
            &scope.root_path,
            cap,
            &mut rules,
            git_mode,
            &mut paths,
            &mut truncated,
            0,
        )?;
        scope.open()?;
        Ok::<_, DaemonError>((scope, paths, truncated))
    })
    .await
    .map_err(|e| DaemonError::Process(e.to_string()))??;
    if let Some(tracked) = tracked {
        let tracked_set: HashSet<_> = tracked.iter().map(String::as_str).collect();
        // Match Git's original order: sorted untracked paths, then cached paths.
        paths.retain(|path| !tracked_set.contains(path.as_str()));
        paths.sort();
        let stdin = paths
            .iter()
            .flat_map(|path| path.bytes().chain([0]))
            .collect();
        if let Some(output) = scope
            .git(&["check-ignore", "-z", "--stdin"], Some(stdin))
            .await?
            && (output.code == Some(0) || output.code == Some(1))
            && !output.truncated
        {
            let ignored: HashSet<_> = nul_fields(&output.stdout).collect();
            paths.retain(|path| !ignored.contains(path.as_str()));
        } else {
            return Err(refuse(write::IO_FAILED, "git ignore policy unavailable"));
        }
        let mut seen = HashSet::new();
        paths.extend(tracked);
        paths.retain(|path| seen.insert(path.clone()));
    }
    if paths.len() > cap {
        paths.truncate(cap);
        truncated = true;
    }
    scope.open()?;
    Ok(ListProjectFilesResponseV1 {
        root: root_string(&scope.root_path),
        paths,
        truncated,
        source: if git_mode {
            ProjectFilesSourceV1::Git
        } else {
            ProjectFilesSourceV1::Walk
        },
    })
}

/// Ignore matchers are built from regular files read through the same pinned
/// handles as the walk. `add_line` never reopens the displayed filename.
struct IgnoreRules {
    git: Vec<Gitignore>,
    ignore: Vec<Gitignore>,
    global: Gitignore,
    gitlinks: HashSet<String>,
}

impl IgnoreRules {
    fn parents(scope: &DirectoryScope) -> Result<Self> {
        let mut rules = Self {
            git: Vec::new(),
            ignore: Vec::new(),
            global: GitignoreBuilder::new("").build_global().0,
            gitlinks: HashSet::new(),
        };
        let mut parents: Vec<_> = scope.scoped.target.ancestors().skip(1).collect();
        parents.reverse();
        for path in parents {
            let directory = AssetDir::open(path).map_err(io_refusal)?;
            rules.push(&directory, path)?;
        }
        Ok(rules)
    }

    fn push(&mut self, directory: &AssetDir, path: &Path) -> Result<()> {
        let metadata_dir = Dir::from_std_file(directory.try_clone_file()?);
        // Git ignore files above a repository boundary no longer apply.
        if metadata_dir.symlink_metadata(".git").is_ok() {
            self.git.clear();
        }
        for (name, matchers) in [(".gitignore", &mut self.git), (".ignore", &mut self.ignore)] {
            let Ok(file) = directory.read_file(Path::new(name)) else {
                continue;
            };
            let mut builder = GitignoreBuilder::new(path);
            for line in std::io::BufReader::new(file.take(MAX_READ_BYTES))
                .lines()
                .map_while(std::result::Result::ok)
            {
                let _ = builder.add_line(Some(path.join(name)), &line);
            }
            if let Ok(matcher) = builder.build() {
                matchers.push(matcher);
            }
        }
        Ok(())
    }

    fn ignored(&self, path: &Path, is_dir: bool, git_mode: bool) -> bool {
        let ignores: &[Gitignore] = if git_mode { &[] } else { &self.ignore };
        for matcher in ignores
            .iter()
            .rev()
            .chain(self.git.iter().rev())
            .chain([&self.global])
        {
            let matched = matcher.matched(path, is_dir);
            if !matched.is_none() {
                return matched.is_ignore();
            }
        }
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    directory: &AssetDir,
    path: &Path,
    root: &Path,
    cap: usize,
    rules: &mut IgnoreRules,
    git_mode: bool,
    paths: &mut Vec<String>,
    truncated: &mut bool,
    depth: usize,
) -> Result<()> {
    if depth >= 256 {
        *truncated = true;
        return Ok(());
    }
    let previous_git = rules.git.clone();
    let ignore_len = rules.ignore.len();
    rules.push(directory, path)?;
    let relative = slash_path(
        path.strip_prefix(root)
            .map_err(|_| refuse(OUTSIDE_ROOT, "walk escaped root"))?,
    );
    if git_mode && rules.gitlinks.contains(&relative) {
        rules.git = previous_git;
        rules.ignore.truncate(ignore_len);
        return Ok(());
    }
    let metadata_dir = Dir::from_std_file(directory.try_clone_file()?);
    if git_mode
        && !relative.is_empty()
        && metadata_dir
            .symlink_metadata(".git")
            .is_ok_and(|m| m.is_dir() || m.is_file())
    {
        paths.push(format!("{relative}/"));
        rules.git = previous_git;
        rules.ignore.truncate(ignore_len);
        return Ok(());
    }
    // Git supplies tracked paths separately, including ignored/deleted files.
    // Pruning ignored directories keeps large build outputs out of the walk;
    // check-ignore applies Git's complete policy to the remaining candidates.
    let mut names: Vec<OsString> = directory.entries(usize::MAX).map_err(io_refusal)?;
    names.sort();
    for name in names {
        if name == ".git" {
            continue;
        }
        let metadata = match metadata_dir.symlink_metadata(&name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_refusal(error)),
        };
        let child = path.join(&name);
        if rules.ignored(&child, metadata.is_dir(), git_mode) {
            continue;
        }
        if metadata.is_dir() {
            #[cfg(test)]
            tests::before_descend();
            let child_dir = directory.open_dir(Path::new(&name)).map_err(io_refusal)?;
            walk(
                &child_dir,
                &child,
                root,
                cap,
                rules,
                git_mode,
                paths,
                truncated,
                depth + 1,
            )?;
        } else if metadata.is_file() || (git_mode && metadata.file_type().is_symlink()) {
            // In Git mode filter before applying the cap; ignored files do not
            // consume the public result limit.
            if !git_mode && paths.len() >= cap {
                *truncated = true;
                break;
            }
            paths.push(slash_path(
                child
                    .strip_prefix(root)
                    .map_err(|_| refuse(OUTSIDE_ROOT, "walk escaped root"))?,
            ));
        }
        if !git_mode && *truncated && paths.len() >= cap {
            break;
        }
    }
    rules.git = previous_git;
    rules.ignore.truncate(ignore_len);
    Ok(())
}

#[cfg(test)]
pub(super) fn take_resolve_hook() -> Option<Box<dyn FnOnce() + Send>> {
    tests::AFTER_RESOLVE.with(|slot| slot.take())
}

#[cfg(test)]
pub(super) fn install_resolve_hook(hook: Option<Box<dyn FnOnce() + Send>>) {
    tests::AFTER_RESOLVE.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
mod tests;
