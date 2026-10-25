// Shared real-repository fixtures for the CLI and Harness custody guards
// (the Harness guard includes this file too).
//
// Every fixture Git invocation goes through `git_os`, a Rust port of the
// `scripts/scratch_git.py` contract: the directory must be absolute, existing
// and physically outside every protected root (this checkout, the working tree
// around the current directory, their git dirs and any GIT_DIR/GIT_WORK_TREE/
// GIT_COMMON_DIR location override); Git runs as `git -C <resolved dir>` with
// every repository-location variable removed and GIT_CEILING_DIRECTORIES set
// to the directory's parent. No scripting runtime is involved.
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const LOCATION_VARIABLES: [&str; 10] = [
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
const OVERRIDE_OPTIONS: [&str; 5] = [
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
];
// Disk-backed locations first: a tmpfs /tmp is not stable for index-byte checks.
const HOST_TEMP_CANDIDATES: [&str; 4] = ["/var/tmp", "/private/tmp", "/tmp", "/dev/shm"];

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Absolute `GIT_CEILING_DIRECTORIES` entries as Git canonicalizes them: an
/// empty entry switches the entries after it to lexical normalization only.
fn ceiling_directories() -> Vec<PathBuf> {
    let Some(value) = std::env::var_os("GIT_CEILING_DIRECTORIES") else {
        return Vec::new();
    };
    let mut ceilings = Vec::new();
    let mut resolve = true;
    for entry in std::env::split_paths(&value) {
        if entry.as_os_str().is_empty() {
            resolve = false;
            continue;
        }
        if !entry.is_absolute() {
            continue;
        }
        if !resolve {
            ceilings.push(lexical_normalize(&entry));
            continue;
        }
        if let Ok(resolved) = entry.canonicalize() {
            ceilings.push(resolved);
        }
    }
    ceilings
}

/// The directory holding `.git` at or above `start`, stopping before a ceiling
/// that is a proper ancestor of `start`.
fn worktree_root(start: &Path, ceilings: &[PathBuf]) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate != start && ceilings.iter().any(|ceiling| ceiling == candidate) {
            return None;
        }
        if candidate.join(".git").exists() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn gitdir_of(root: &Path) -> Option<PathBuf> {
    let entry = root.join(".git");
    if entry.is_dir() {
        return entry.canonicalize().ok();
    }
    let text = std::fs::read_to_string(&entry).ok()?;
    let target = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    let target = if target.is_absolute() {
        target
    } else {
        root.join(target)
    };
    Some(target.canonicalize().unwrap_or(target))
}

fn resolve_lenient(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn protected_roots() -> Vec<PathBuf> {
    let mut roots = vec![resolve_lenient(
        Path::new(rsi_common_manifest_dir!()).join("../.."),
    )];
    let current = std::env::current_dir()
        .and_then(|cwd| cwd.canonicalize())
        .ok()
        .and_then(|cwd| worktree_root(&cwd, &ceiling_directories()));
    roots.extend(current);
    let gitdirs: Vec<PathBuf> = roots.iter().filter_map(|root| gitdir_of(root)).collect();
    roots.extend(gitdirs);
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
        let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) else {
            continue;
        };
        roots.push(resolve_lenient(PathBuf::from(value)));
    }
    roots.dedup();
    roots
}

fn inside_protected(resolved: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| resolved.starts_with(root))
}

/// The resolved physical scratch path; panics (before Git starts) when unsafe.
fn validate_scratch_dir(directory: &Path) -> PathBuf {
    assert!(
        directory.is_absolute(),
        "scratch directory is not absolute: {directory:?}"
    );
    let resolved = directory.canonicalize().unwrap_or_else(|error| {
        panic!("scratch directory does not exist: {directory:?} ({error})")
    });
    assert!(
        resolved.is_dir(),
        "scratch path is not a directory: {directory:?}"
    );
    assert!(
        resolved.parent().is_some(),
        "scratch directory is the filesystem root"
    );
    let roots = protected_roots();
    assert!(
        !inside_protected(&resolved, &roots),
        "scratch directory {directory:?} resolves to {resolved:?}, inside a protected root; Git there would act on the assigned sandbox"
    );
    resolved
}

/// A writable existing temporary directory outside every protected root.
fn host_temp_root() -> PathBuf {
    let roots = protected_roots();
    let candidates =
        std::iter::once(std::env::temp_dir()).chain(HOST_TEMP_CANDIDATES.iter().map(PathBuf::from));
    for candidate in candidates {
        let Ok(resolved) = candidate.canonicalize() else {
            continue;
        };
        if !resolved.is_dir() || inside_protected(&resolved, &roots) {
            continue;
        }
        if tempfile::Builder::new().tempdir_in(&resolved).is_err() {
            continue;
        }
        return resolved;
    }
    panic!("no temporary directory outside the protected roots is available");
}

pub(super) fn scratch_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rsi-custody-")
        .tempdir_in(host_temp_root())
        .unwrap()
}

/// Panic when a global option before the subcommand would move Git off the
/// validated directory (`git commit -C <rev>` is not one).
fn reject_location_overrides(args: &[&OsStr]) {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].to_string_lossy();
        assert!(
            !(OVERRIDE_OPTIONS.contains(&&*arg)
                || arg.starts_with("--git-dir=")
                || arg.starts_with("--work-tree=")),
            "git option {arg:?} would override the scratch directory"
        );
        if arg == "-c" || arg == "--config-env" {
            index += 2;
            continue;
        }
        if !arg.starts_with('-') {
            return;
        }
        index += 1;
    }
}

/// `git -C <validated dir> <args>` with location variables scrubbed; returns
/// trimmed stdout and asserts success.
fn git_os(dir: &Path, args: &[&OsStr]) -> String {
    reject_location_overrides(args);
    let resolved = validate_scratch_dir(dir);
    let mut command = Command::new("git");
    command.arg("-C").arg(&resolved).args(args);
    for name in LOCATION_VARIABLES {
        command.env_remove(name);
    }
    command.env(
        "GIT_CEILING_DIRECTORIES",
        resolved.parent().map(Path::as_os_str).unwrap_or_default(),
    );
    let output = command.output().expect("run scratch git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[allow(dead_code)]
pub(super) fn git(dir: &Path, args: &[&str]) -> String {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    git_os(dir, &args)
}

macro_rules! g {
    ($dir:expr, $($arg:expr),+ $(,)?) => {
        git_os($dir, &[$(AsRef::<OsStr>::as_ref(&$arg)),+])
    };
}

pub(super) struct ExternalFixture {
    pub root: tempfile::TempDir,
    pub sandbox: PathBuf,
    pub base: PathBuf,
    pub refused: Vec<(String, String, String)>,
    allowed: Vec<String>,
    shipped: bool,
}

struct Layout {
    sandbox: PathBuf,
    base: PathBuf,
    refused: Vec<(String, String, String)>,
    allowed: Vec<String>,
    shipped: bool,
}

fn refusal(label: &str, command: String, reason: &str) -> (String, String, String) {
    (label.to_owned(), command, reason.to_owned())
}

/// Real-Git layout plus the refused/allowed command tables.
fn populate(root: &Path) -> Layout {
    let root = validate_scratch_dir(root);
    let shared = root.join("rsi");
    let base = root.join("sandboxes");
    let scratch = root.join("scratch");
    let sandbox = base.join("assigned");
    std::fs::create_dir(&shared).unwrap();
    std::fs::create_dir(&base).unwrap();
    g!(&shared, "init", "-q", "-b", "main");
    g!(&shared, "config", "gc.auto", "0");
    g!(
        &shared,
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "fixture"
    );
    g!(&shared, "worktree", "add", "-q", "-b", "assigned", &sandbox);
    g!(&shared, "clone", "-q", "--no-local", &sandbox, &scratch);
    let other = base.join("other");
    let linked = root.join("linked");
    let external = root.join("external");
    g!(&shared, "clone", "-q", "--no-local", &sandbox, &other);
    g!(&shared, "worktree", "add", "-q", "--detach", &linked);
    g!(&shared, "clone", "-q", "--no-local", &sandbox, &external);
    g!(
        &external,
        "worktree",
        "add",
        "-q",
        "--detach",
        base.join("external-worktree")
    );
    let redirected = root.join("redirected");
    std::fs::create_dir(&redirected).unwrap();
    std::fs::write(
        redirected.join(".git"),
        format!("gitdir: {}\n", shared.join(".git").display()),
    )
    .unwrap();
    std::fs::create_dir(scratch.join("sub")).unwrap();

    let mut refused = Vec::new();
    for (label, directory) in [
        ("assigned", &sandbox),
        ("other sandbox", &other),
        ("shared checkout", &shared),
        ("common-dir", &linked),
        ("worktree identity", &external),
        ("redirected gitdir", &redirected),
    ] {
        refused.push(refusal(
            label,
            format!(
                "git -C {} subtree split --prefix=d -b escaped HEAD",
                directory.display()
            ),
            "independent scratch repository",
        ));
    }
    let scratch_display = scratch.display();
    for (label, chain) in [
        ("relative first", "-C scratch".to_owned()),
        ("relative second", format!("-C {scratch_display} -C sub")),
        ("variable first", "-C \"$SCRATCH\"".to_owned()),
        (
            "variable second",
            format!("-C {scratch_display} -C \"$PART\""),
        ),
    ] {
        refused.push(refusal(
            label,
            format!("git {chain} subtree split --prefix=d -b escaped HEAD"),
            "not a literal absolute directory",
        ));
    }
    for (label, command) in [
        ("env reset", format!("env -i git -C {scratch_display}")),
        (
            "env unset exec path",
            format!("env -u GIT_EXEC_PATH git -C {scratch_display}"),
        ),
        (
            "env unset PATH",
            format!("env --unset=PATH git -C {scratch_display}"),
        ),
        (
            "exec-path equals",
            format!("git --exec-path=/custom -C {scratch_display}"),
        ),
        (
            "exec-path separate",
            format!("git --exec-path /custom -C {scratch_display}"),
        ),
        (
            "GIT_EXEC_PATH assignment",
            format!("GIT_EXEC_PATH=/custom git -C {scratch_display}"),
        ),
        (
            "GIT_EXEC_PATH env",
            format!("env GIT_EXEC_PATH=/custom git -C {scratch_display}"),
        ),
        (
            "GIT_EXEC_PATH export",
            format!("export GIT_EXEC_PATH=/custom; git -C {scratch_display}"),
        ),
        (
            "PATH assignment",
            format!("PATH=/custom git -C {scratch_display}"),
        ),
    ] {
        refused.push(refusal(
            label,
            format!("{command} subtree split --prefix=d HEAD"),
            "exact executable cannot be checked",
        ));
    }
    let replacement = root.join("git-subtree");
    std::fs::write(&replacement, "#!/bin/sh\nexit 0\n").unwrap();
    make_executable(&replacement);
    refused.push(refusal(
        "PATH git-NAME",
        format!(
            "cd {scratch_display} && PATH={} git-subtree split --prefix=d -b escaped HEAD",
            root.display()
        ),
        "direct git-NAME executable",
    ));
    refused.push(refusal(
        "absolute git-NAME",
        format!(
            "cd {scratch_display} && {} split --prefix=d HEAD",
            replacement.display()
        ),
        "direct git-NAME executable",
    ));

    let exec_path = PathBuf::from(g!(&scratch, "--exec-path"));
    let shipped = ["git-subtree", "git-subtree.exe"]
        .iter()
        .any(|name| exec_path.join(name).is_file());
    let allowed = vec![
        format!("git -C {scratch_display} subtree split --prefix=d -b desktop-only HEAD"),
        format!("cd {scratch_display} && git subtree split --prefix=d -b desktop-only HEAD"),
        format!(
            "git -C {scratch_display} -C {scratch_display} subtree split --prefix=d -b desktop-only HEAD"
        ),
        "git subtree split --prefix=d HEAD".to_owned(),
        format!("git -C {} subtree split --prefix=d HEAD", sandbox.display()),
    ];
    Layout {
        sandbox,
        base,
        refused,
        allowed,
        shipped,
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

impl ExternalFixture {
    pub fn new() -> Self {
        let root = scratch_tempdir();
        let layout = populate(root.path());
        Self {
            sandbox: layout.sandbox,
            base: layout.base,
            refused: layout.refused,
            allowed: layout.allowed,
            shipped: layout.shipped,
            root,
        }
    }

    pub fn shipped_subtree(&self) -> bool {
        self.shipped
    }
    pub fn allowed(&self) -> Vec<String> {
        self.allowed.clone()
    }

    pub fn symlink(&self, target: &Path, name: &str) -> PathBuf {
        let link = self.root.path().join(name);
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(target, &link).unwrap();
        link
    }
}
