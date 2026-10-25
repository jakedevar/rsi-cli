//! #446 / #453: `scripts/target-pool` in Rust, so a baseline replay starts from the
//! warm pool instead of a cold build and the pool lifecycle needs no interpreter.
//!
//! This module is the read side and the dispatcher: `seed` (choose the nearest ready
//! ancestor pool, hard-link its artifacts into an empty sandbox target, normalise
//! source mtimes so Cargo rebuilds exactly what differs; `--build-missing` asks for a
//! detached build) and `key`. The rest lives beside it: `target_pool_ops` (gc,
//! reclaim-idle, refresh, status, summary, disk floors), `target_pool_build` (build,
//! the detached request, seed-from) and `target_pool_cli` (their command lines).
//!
//! The pool key, marker names, lock files, seed marker and printed outcome are
//! byte-compatible with the Python tool, because pools it built are read here and
//! pools built here must stay readable by it.
use crate::target_pool_ops::DEFAULT_REF;
use crate::{Ctx, cli, target_pool_build, target_pool_cli, target_pool_ops, touched_shards::modes};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const RECIPE_VERSION: u64 = 1;
pub const BUILD_COMMAND: [&str; 4] = ["cargo", "test", "--workspace", "--no-run"];
pub const DEFAULT_MAX_DISTANCE: u64 = 200;
pub const OLD_MTIME: i64 = 946_684_800;
pub const SEED_MARKER: &str = ".rsi-target-pool.json";
pub const REPO_FILE: &str = "repo-path";
pub(crate) const KEY_ENV_EXACT: [&str; 8] = [
    "CARGO_INCREMENTAL",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_BUILD_TARGET",
    "CARGO_TARGET_APPLIES_TO_HOST",
    "RUSTUP_TOOLCHAIN",
];
pub(crate) const KEY_ENV_PREFIX: &str = "CARGO_PROFILE_";
const MANIFEST_ENV_DEPS: [&str; 2] = [
    "# env-dep:CARGO_MANIFEST_DIR",
    "# env-dep:CARGO_MANIFEST_PATH",
];
/// Entries a fresh sandbox target may already hold (daemon scratch and markers).
const EMPTY_TARGET_ENTRIES: [&str; 3] = [".rsi-tmp", ".rsi-build-env.json", "CACHEDIR.TAG"];
pub(crate) const GIT_CONTEXT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];
const CAPABILITY_PROBE: Duration = Duration::from_secs(10);

pub type Env = BTreeMap<String, String>;

pub(crate) fn env_of(ctx: &Ctx) -> Env {
    ctx.vars()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

// ---- Git (the tool's own, with an inherited worktree context removed) ----

pub struct Git<'a> {
    pub env: &'a Env,
}
impl Git<'_> {
    pub fn command(&self, repo: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(repo).args(args);
        command.env_clear().envs(self.env);
        for name in GIT_CONTEXT_ENV {
            command.env_remove(name);
        }
        command
    }
    /// stdout (stripped); an error when Git exits nonzero.
    pub fn out(&self, repo: &Path, args: &[&str]) -> io::Result<String> {
        let (ok, stdout, stderr) = self.run(repo, args)?;
        if ok {
            Ok(stdout.trim().to_string())
        } else {
            Err(io::Error::other(format!(
                "git {}: {}",
                args.join(" "),
                stderr.trim()
            )))
        }
    }
    /// stdout (stripped) or empty when Git exits nonzero (`check=False`).
    pub fn lenient(&self, repo: &Path, args: &[&str]) -> String {
        match self.run(repo, args) {
            Ok((true, stdout, _)) => stdout.trim().to_string(),
            _ => String::new(),
        }
    }
    pub fn run(&self, repo: &Path, args: &[&str]) -> io::Result<(bool, String, String)> {
        let output = self.command(repo, args).output()?;
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
    pub fn succeeds(&self, repo: &Path, args: &[&str]) -> bool {
        self.command(repo, args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

// ---- Pool directory, key and markers ----

pub fn pool_root(env: &Env) -> io::Result<PathBuf> {
    if let Some(configured) = env.get("RSI_TARGET_POOL_ROOT").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(configured));
    }
    let home = env
        .get("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| io::Error::other("HOME is not set and RSI_TARGET_POOL_ROOT is empty"))?;
    Ok(Path::new(home).join(".rsi").join("target-pool"))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn repo_dir(git: &Git, env: &Env, repo: &Path) -> io::Result<PathBuf> {
    let common = git.out(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = fs::canonicalize(&common).unwrap_or_else(|_| PathBuf::from(&common));
    let digest = Sha256::digest(common.to_string_lossy().as_bytes());
    Ok(pool_root(env)?.join(&hex(&digest)[..12]))
}

pub fn build_key(directory: &Path, env: &Env) -> io::Result<String> {
    let rustc = Command::new("rustc")
        .arg("-vV")
        .current_dir(directory)
        .env_clear()
        .envs(env)
        .output()?;
    if !rustc.status.success() {
        return Err(io::Error::other(format!(
            "rustc -vV failed: {}",
            String::from_utf8_lossy(&rustc.stderr).trim()
        )));
    }
    let settings: Map<String, Value> = env
        .iter()
        .filter(|(name, _)| {
            KEY_ENV_EXACT.contains(&name.as_str()) || name.starts_with(KEY_ENV_PREFIX)
        })
        .map(|(name, value)| (name.clone(), json!(value)))
        .collect();
    // `json.dumps({...}, sort_keys=True)`: the material the Python tool hashed.
    let material = modes::dumps_sorted(&json!({
        "recipe": RECIPE_VERSION,
        "command": BUILD_COMMAND,
        "rustc": String::from_utf8_lossy(&rustc.stdout),
        "env": settings,
    }));
    Ok(hex(&Sha256::digest(material.as_bytes()))[..12].to_string())
}

/// True when `RUSTC_WRAPPER` is an `rsi-build-rustc` that unlinks shared outputs.
pub(crate) fn wrapper_unlinks_outputs(env: &Env) -> bool {
    let wrapper = env.get("RUSTC_WRAPPER").map(String::as_str).unwrap_or("");
    let name = Path::new(wrapper)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name != "rsi-build-rustc" && name != "rsi-build-rustc.exe" {
        return false;
    }
    let Ok(mut child) = Command::new(wrapper)
        .arg("--capabilities")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + CAPABILITY_PROBE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut text = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = io::Read::read_to_string(&mut pipe, &mut text);
                }
                return status.success() && text.split_whitespace().any(|w| w == "unlink-outputs");
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub(crate) fn is_ready(record: &Value) -> bool {
    record["state"] == "ready"
        && record["target"]
            .as_str()
            .is_some_and(|t| !t.is_empty() && Path::new(t).is_dir())
}

pub(crate) fn ready_pools(directory: &Path, key: &str) -> Vec<Value> {
    let Ok(entries) = fs::read_dir(directory) else {
        return vec![];
    };
    let suffix = format!("-{key}.json");
    let mut names: Vec<_> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(&suffix))
        .collect();
    names.sort();
    let mut pools: Vec<Value> = names
        .iter()
        .filter_map(|name| {
            let text = fs::read_to_string(directory.join(name)).ok()?;
            serde_json::from_str::<Value>(&text).ok()
        })
        .filter(is_ready)
        .collect();
    let built = |r: &Value| r["built_at"].as_f64().unwrap_or(0.0);
    pools.sort_by(|a, b| built(b).total_cmp(&built(a)));
    pools
}

/// Commits `sha` is behind `head`; `None` when `sha` is not an ancestor of it.
pub(crate) fn pool_distance(git: &Git, sandbox: &Path, head: &str, sha: &str) -> Option<u64> {
    if sha == head {
        return Some(0);
    }
    if !git.succeeds(sandbox, &["merge-base", "--is-ancestor", sha, head]) {
        return None;
    }
    git.out(sandbox, &["rev-list", "--count", &format!("{sha}..{head}")])
        .ok()?
        .parse()
        .ok()
}

pub(crate) fn choose_pool(
    git: &Git,
    sandbox: &Path,
    head: &str,
    pools: &[Value],
    max_distance: u64,
) -> Option<Value> {
    let mut best: Option<(u64, &Value)> = None;
    for record in pools {
        let Some(distance) = record["sha"]
            .as_str()
            .and_then(|sha| pool_distance(git, sandbox, head, sha))
        else {
            continue;
        };
        if distance == 0 {
            return Some(with_distance(record, 0));
        }
        if distance <= max_distance && best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, record));
        }
    }
    best.map(|(distance, record)| with_distance(record, distance))
}

pub(crate) fn with_distance(record: &Value, distance: u64) -> Value {
    let mut chosen = record.clone();
    chosen["distance"] = json!(distance);
    chosen
}

/// Why `choose_pool` found nothing; reporting only.
pub(crate) fn explain_no_pool(
    git: &Git,
    sandbox: &Path,
    head: &str,
    pools: &[Value],
    max_distance: u64,
) -> Value {
    if pools.is_empty() {
        return json!({"reason": "no ready pool has this build key"});
    }
    let mut nearest: Option<(String, u64)> = None;
    for record in pools {
        let sha = record["sha"].as_str().unwrap_or_default();
        if let Some(distance) = pool_distance(git, sandbox, head, sha) {
            if nearest.as_ref().is_none_or(|(_, d)| distance < *d) {
                nearest = Some((sha.to_string(), distance));
            }
        }
    }
    match nearest {
        None => {
            json!({"reason": format!("none of {} ready pool(s) is an ancestor of HEAD", pools.len())})
        }
        Some((sha, distance)) => json!({
            "reason": format!("nearest ancestor pool is {distance} commits behind (limit {max_distance})"),
            "nearest_pool": {"sha": sha, "distance": distance},
        }),
    }
}

// ---- Locks: a pool is never removed while a seed reads it ----

/// `<pool>.use`, shared and non-blocking; `None` when gc holds it exclusively.
pub(crate) fn shared_use_lock(directory: &Path, name: &str) -> Option<File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(directory.join(format!("{name}.use")))
        .ok()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this function owns for the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
            return None;
        }
    }
    Some(file)
}

/// Choose a pool and hold its shared use lock; a pool gc is removing is not
/// offered again.
pub(crate) fn claim_pool(
    directory: &Path,
    pools: &[Value],
    choose: impl Fn(&[Value]) -> Option<Value>,
) -> Option<(Value, File)> {
    let mut candidates = pools.to_vec();
    loop {
        let chosen = choose(&candidates)?;
        let name = chosen["name"].as_str().unwrap_or_default().to_string();
        if let Some(handle) = shared_use_lock(directory, &name) {
            if chosen["target"]
                .as_str()
                .is_some_and(|t| Path::new(t).is_dir())
            {
                return Some((chosen, handle));
            }
        }
        candidates.retain(|r| r["name"].as_str() != Some(&name));
    }
}

// ---- Seeding ----

pub(crate) fn target_is_empty(target: &Path) -> io::Result<bool> {
    if !target.exists() {
        return Ok(true);
    }
    Ok(fs::read_dir(target)?
        .flatten()
        .all(|e| EMPTY_TARGET_ENTRIES.contains(&e.file_name().to_string_lossy().as_ref())))
}

/// (profile, unit stem) for every unit whose rustc dep-info read the manifest path.
fn manifest_baking_units(pool_target: &Path) -> BTreeSet<(String, String)> {
    let mut units = BTreeSet::new();
    let Ok(profiles) = fs::read_dir(pool_target) else {
        return units;
    };
    for profile in profiles.flatten() {
        let deps = profile.path().join("deps");
        let Ok(files) = fs::read_dir(&deps) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".d") else {
                continue;
            };
            let Ok(bytes) = fs::read(file.path()) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            if MANIFEST_ENV_DEPS.iter().any(|m| text.contains(m)) {
                units.insert((
                    profile.file_name().to_string_lossy().into_owned(),
                    stem.to_string(),
                ));
            }
        }
    }
    units
}

/// The `-<hash>` extra filename that makes a `<crate>-<hash>` stem unique.
fn unit_hash(stem: &str) -> String {
    match stem.rsplit_once('-') {
        Some((_, hash)) => format!("-{hash}"),
        None => stem.to_string(),
    }
}

/// Whether a deps/ file is an output of one of these units. rustc shortens a
/// too-long output name to `<hash>-<tail>`, keeping `-<unit hash>.`.
fn belongs_to(name: &str, stems: &BTreeSet<String>, hashes: &BTreeSet<String>) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    if stems.contains(stem) || stem.strip_prefix("lib").is_some_and(|s| stems.contains(s)) {
        return true;
    }
    hashes.iter().any(|unit| name.contains(&format!("{unit}.")))
}

fn link_kind(name: &str, link_in_place: bool) -> &'static str {
    if name.ends_with(".d") {
        "copy"
    } else if name.ends_with(".rlib") || name.ends_with(".rmeta") || link_in_place {
        "link"
    } else {
        "copy"
    }
}

/// `shutil.copy2(follow_symlinks=False)` plus owner-writable.
fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(source)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(link, destination)?;
        #[cfg(not(unix))]
        {
            let _ = link;
            return Err(io::Error::other("symlinks in a target pool need Unix"));
        }
        return Ok(());
    }
    fs::copy(source, destination)?;
    // The copy has the source's mode, which for a pool's in-place output is read-only: make it
    // owner-writable before it is opened to restore the times (`copystat` then `chmod`).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(destination)?.permissions().mode();
        fs::set_permissions(destination, fs::Permissions::from_mode(mode | 0o200))?;
    }
    let copy = fs::OpenOptions::new().write(true).open(destination)?;
    let mut times = fs::FileTimes::new();
    if let Ok(accessed) = metadata.accessed() {
        times = times.set_accessed(accessed);
    }
    if let Ok(modified) = metadata.modified() {
        times = times.set_modified(modified);
    }
    copy.set_times(times)?;
    drop(copy);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(destination)?.permissions().mode();
        fs::set_permissions(destination, fs::Permissions::from_mode(mode | 0o200))?;
    }
    Ok(())
}

#[derive(Default)]
struct Walk {
    /// (directory, its non-directory entries) in `os.walk` order.
    roots: Vec<(PathBuf, Vec<String>)>,
}
/// `os.walk(top)` without following symlinks: a symlink to a directory is a
/// "directory" entry that is not descended into and not copied.
fn walk(top: &Path, out: &mut Walk) -> io::Result<()> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(top)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            if !fs::symlink_metadata(&path)?.file_type().is_symlink() {
                dirs.push(path);
            }
        } else {
            files.push(name);
        }
    }
    out.roots.push((top.to_path_buf(), files));
    for dir in dirs {
        walk(&dir, out)?;
    }
    Ok(())
}

pub(crate) fn seed_tree(pool_target: &Path, target: &Path, link_in_place: bool) -> io::Result<Value> {
    let skipped_units = manifest_baking_units(pool_target);
    let (mut links, mut copies, mut skipped) = (0u64, 0u64, 0u64);
    #[cfg(unix)]
    let mut linked_inodes = BTreeMap::<(u64, u64), PathBuf>::new();
    let mut profiles: Vec<PathBuf> = fs::read_dir(pool_target)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("deps").is_dir())
        .collect();
    profiles.sort();
    for top in ["CACHEDIR.TAG", ".rustc_info.json"] {
        if pool_target.join(top).is_file() && !target.join(top).exists() {
            copy_file(&pool_target.join(top), &target.join(top))?;
        }
    }
    for profile in &profiles {
        let profile_name = profile.file_name().unwrap_or_default().to_string_lossy();
        let stems: BTreeSet<String> = skipped_units
            .iter()
            .filter(|(owner, _)| *owner == profile_name)
            .map(|(_, stem)| stem.clone())
            .collect();
        let hashes: BTreeSet<String> = stems.iter().map(|s| unit_hash(s)).collect();
        let mut roots = Walk::default();
        walk(profile, &mut roots)?;
        // deps/ first, so uplifted copies in the profile root can follow inodes.
        roots.roots.sort_by(|a, b| {
            let key = |p: &Path| {
                (
                    p.file_name().is_none_or(|n| n != "deps"),
                    p.to_string_lossy().into_owned(),
                )
            };
            key(&a.0).cmp(&key(&b.0))
        });
        for (root, files) in &roots.roots {
            let relative = root.strip_prefix(pool_target).map_err(io::Error::other)?;
            fs::create_dir_all(target.join(relative))?;
            let in_deps = root.file_name().is_some_and(|n| n == "deps")
                && root.parent() == Some(profile.as_path());
            for name in files {
                if name == ".cargo-lock" {
                    continue;
                }
                let source = root.join(name);
                let destination = target.join(relative).join(name);
                if in_deps && belongs_to(name, &stems, &hashes) {
                    skipped += 1;
                    continue;
                }
                let status = fs::symlink_metadata(&source)?;
                #[cfg(unix)]
                let inode = {
                    use std::os::unix::fs::MetadataExt;
                    (status.dev(), status.ino())
                };
                let plain = !status.file_type().is_symlink();
                if in_deps && plain && link_kind(name, link_in_place) == "link" {
                    fs::hard_link(&source, &destination)?;
                    #[cfg(unix)]
                    linked_inodes.insert(inode, destination);
                    links += 1;
                    continue;
                }
                #[cfg(unix)]
                if root == profile && linked_inodes.contains_key(&inode) {
                    // Cargo's uplifted copy (target/debug/<bin>) is a hard link of a
                    // deps/ file; keep it one inode.
                    fs::hard_link(&source, &destination)?;
                    links += 1;
                    continue;
                }
                copy_file(&source, &destination)?;
                copies += 1;
            }
        }
    }
    Ok(json!({
        "linked": links, "copied": copies, "skipped_outputs": skipped,
        "rebuilt_units": skipped_units.iter().map(|(_, stem)| stem.clone()).collect::<BTreeSet<_>>(),
    }))
}

/// `os.utime(path, (stamp, stamp), follow_symlinks=False)`; a missing path is `Ok(false)`.
fn set_times(path: &Path, stamp: f64, follow: bool) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_path =
            std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let whole = stamp.floor();
        let time = libc::timespec {
            tv_sec: whole as libc::time_t,
            tv_nsec: ((stamp - whole) * 1e9) as _,
        };
        let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
        // SAFETY: a valid NUL-terminated path and a two-element timespec array.
        let status = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c_path.as_ptr(),
                [time, time].as_ptr(),
                flags,
            )
        };
        if status == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::NotFound => Ok(false),
            _ => Err(error),
        }
    }
    #[cfg(not(unix))]
    {
        let moment = UNIX_EPOCH + Duration::from_secs_f64(stamp.max(0.0));
        match File::options().write(true).open(path) {
            Ok(file) => file
                .set_times(
                    fs::FileTimes::new()
                        .set_accessed(moment)
                        .set_modified(moment),
                )
                .map(|()| true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn split_nul(text: &str) -> impl Iterator<Item = &str> {
    text.split('\0')
}

pub(crate) fn normalise_mtimes(git: &Git, sandbox: &Path, pool_sha: &str) -> io::Result<Value> {
    let mut changed: BTreeSet<String> =
        split_nul(&git.out(sandbox, &["diff", "--name-only", "-z", pool_sha, "HEAD"])?)
            .map(str::to_owned)
            .collect();
    let status = git.out(
        sandbox,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    for record in split_nul(&status) {
        if record.chars().count() > 3 {
            changed.insert(record.chars().skip(3).collect());
        }
    }
    changed.remove("");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs_f64();
    let (mut unchanged, mut fresh) = (0u64, 0u64);
    let tracked: Vec<String> = split_nul(&git.out(sandbox, &["ls-files", "-z"])?)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect();
    for name in &tracked {
        let stamp = if changed.contains(name) {
            now
        } else {
            OLD_MTIME as f64
        };
        if !set_times(&sandbox.join(name), stamp, false)? {
            continue;
        }
        if stamp == OLD_MTIME as f64 {
            unchanged += 1;
        } else {
            fresh += 1;
        }
    }
    for name in &changed {
        let path = sandbox.join(name);
        let plain = fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_symlink());
        if plain {
            set_times(&path, now, true)?;
        }
    }
    // A build script may watch a directory (`rerun-if-changed=src/migrations`) and
    // Cargo then compares the directory's own mtime, which is the checkout time.
    // Directories get the same rule as files: old unless a changed path sits
    // directly in them. Files first, since setting a file's times never touches
    // its directory.
    let parent_of = |name: &str| -> String {
        Path::new(name).parent().map_or_else(
            || ".".into(),
            |p| {
                let text = p.to_string_lossy().into_owned();
                if text.is_empty() { ".".into() } else { text }
            },
        )
    };
    let touched: BTreeSet<String> = changed.iter().map(|n| parent_of(n)).collect();
    let mut directories = BTreeSet::<String>::new();
    for name in &tracked {
        let mut parent = parent_of(name);
        while parent != "." && !directories.contains(&parent) {
            directories.insert(parent.clone());
            parent = parent_of(&parent);
        }
    }
    let mut ordered: Vec<_> = directories.into_iter().collect();
    ordered.sort_by_key(|d| std::cmp::Reverse(d.len()));
    for name in ordered {
        let stamp = if touched.contains(&name) {
            now
        } else {
            OLD_MTIME as f64
        };
        set_times(&sandbox.join(&name), stamp, false)?;
    }
    Ok(json!({"unchanged_files": unchanged, "changed_files": fresh}))
}

pub(crate) fn main_worktree(git: &Git, path: &Path) -> Option<PathBuf> {
    let listing = git.lenient(path, &["worktree", "list", "--porcelain"]);
    let first = listing.lines().next()?;
    let main = PathBuf::from(first.strip_prefix("worktree ")?);
    main.join("Cargo.toml").is_file().then_some(main)
}

pub(crate) fn known_repo(directory: &Path) -> Option<PathBuf> {
    let repo = PathBuf::from(fs::read_to_string(directory.join(REPO_FILE)).ok()?.trim());
    repo.join("Cargo.toml").is_file().then_some(repo)
}

/// Record the checkout this directory's pools belong to, so a refresh can rebuild
/// after gc removed the last pool.
pub(crate) fn remember_repo(git: &Git, directory: &Path, path: &Path) {
    let main = if path.is_dir() {
        main_worktree(git, path)
    } else {
        None
    };
    if let Some(main) = main {
        if known_repo(directory).as_deref() != Some(main.as_path()) {
            let _ = fs::write(
                directory.join(REPO_FILE),
                format!("{}\n", main.to_string_lossy()),
            );
        }
    }
}

pub(crate) fn failed_pool(directory: &Path, head: &str, key: &str) -> Option<Value> {
    let text = fs::read_to_string(directory.join(format!("{head}-{key}.json"))).ok()?;
    let record: Value = serde_json::from_str(&text).ok()?;
    (record["state"] == "failed").then_some(record)
}

// ---- JSON as the Python tool printed it ----

/// `json.dumps(value)` (insertion order, `", "` / `": "`) or, with an indent,
/// `json.dumps(value, indent=N)`; ASCII-escaped either way.
pub fn dumps(value: &Value, indent: Option<usize>) -> String {
    fn go(value: &Value, indent: Option<usize>, depth: usize, out: &mut String) {
        let (open_gap, item_gap, close_gap) = match indent {
            Some(n) => (
                format!("\n{}", " ".repeat(n * (depth + 1))),
                format!(",\n{}", " ".repeat(n * (depth + 1))),
                format!("\n{}", " ".repeat(n * depth)),
            ),
            None => (String::new(), ", ".to_string(), String::new()),
        };
        match value {
            Value::Object(map) if !map.is_empty() => {
                out.push('{');
                out.push_str(&open_gap);
                for (i, (key, item)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&item_gap);
                    }
                    out.push_str(&modes::python_json(&Value::String(key.clone()), false));
                    out.push_str(": ");
                    go(item, indent, depth + 1, out);
                }
                out.push_str(&close_gap);
                out.push('}');
            }
            Value::Array(items) if !items.is_empty() => {
                out.push('[');
                out.push_str(&open_gap);
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&item_gap);
                    }
                    go(item, indent, depth + 1, out);
                }
                out.push_str(&close_gap);
                out.push(']');
            }
            other => out.push_str(&modes::python_json(other, false)),
        }
    }
    let mut out = String::new();
    go(value, indent, 0, &mut out);
    out
}

// ---- Commands ----

pub struct SeedOptions {
    pub sandbox: PathBuf,
    pub target: Option<PathBuf>,
    pub max_distance: u64,
    /// `--build-missing`: ask for a detached pool build when the seed found none near.
    pub build_missing: bool,
    /// The command prefix that runs `target-pool` (this binary and its subcommand name),
    /// used to start the detached build.
    pub build_program: Vec<String>,
}

/// `target-pool seed`: the printed outcome (also the seed marker's content).
pub fn seed(options: &SeedOptions, env: &Env) -> io::Result<Value> {
    let git = Git { env };
    let sandbox = fs::canonicalize(&options.sandbox)?;
    let target = match &options.target {
        Some(target) => absolute(target)?,
        None => sandbox.join("target"),
    };
    let mut outcome = Map::new();
    outcome.insert("sandbox".into(), json!(sandbox));
    outcome.insert("target".into(), json!(target));
    let skip = |mut outcome: Map<String, Value>, why: &str| {
        outcome.insert("result".into(), json!(why));
        Ok(Value::Object(outcome))
    };
    if !sandbox.join("Cargo.toml").is_file() {
        return skip(outcome, "skipped: not a Cargo workspace");
    }
    if !target_is_empty(&target)? {
        return skip(outcome, "skipped: target is not empty");
    }
    let head = git.out(&sandbox, &["rev-parse", "HEAD"])?;
    let key = build_key(&sandbox, env)?;
    let directory = repo_dir(&git, env, &sandbox)?;
    // Pools imported before #2717 have no recorded checkout; the first seed of a
    // sandbox of the repository records it (a no-op when already known).
    remember_repo(&git, &directory, &sandbox);
    let pools = ready_pools(&directory, &key);
    let claimed = claim_pool(&directory, &pools, |left| {
        choose_pool(&git, &sandbox, &head, left, options.max_distance)
    });
    outcome.insert("head".into(), json!(head));
    outcome.insert("key".into(), json!(key));
    if let Some(failure) = failed_pool(&directory, &head, &key) {
        // Surface the durable failure on later launches rather than requiring
        // the manager to find the unit.
        outcome.insert("build_failure".into(), failure);
    }
    if options.build_missing {
        let rolling_tip = git.lenient(&sandbox, &["rev-parse", "--verify", DEFAULT_REF]);
        let distance = claimed.as_ref().map(|(chosen, _)| chosen["distance"].as_u64().unwrap_or(0));
        match distance {
            None => {
                let result = target_pool_build::request_build(&git, &sandbox, &head, &options.build_program)?;
                outcome.insert("build".into(), json!(result));
            }
            Some(d) if head == rolling_tip && d > 0 => {
                let result = target_pool_build::request_build(&git, &sandbox, &head, &options.build_program)?;
                outcome.insert("build".into(), json!(result));
            }
            Some(d) if d > 0 => {
                let detail = format!("nearby pool seeded at distance {d}; not building a one-off pool");
                let pool_sha = claimed.as_ref().map_or(Value::Null, |(chosen, _)| chosen["sha"].clone());
                target_pool_ops::record_event(
                    &directory,
                    "skip",
                    target_pool_ops::fields(json!({
                        "reason": "pool_skipped_nearby", "detail": detail, "head": head,
                        "pool_sha": pool_sha, "distance": d,
                    })),
                );
                outcome.insert("build".into(), json!(format!("skipped: {detail}")));
            }
            Some(_) => {}
        }
    }
    let Some((chosen, in_use)) = claimed else {
        if let Value::Object(why) =
            explain_no_pool(&git, &sandbox, &head, &pools, options.max_distance)
        {
            outcome.extend(why);
        }
        return skip(outcome, "no pool");
    };
    let started = Instant::now();
    fs::create_dir_all(&target)?;
    // Stage on the same filesystem and move into place last: a seed that is
    // killed leaves an empty target (a cold build), never a partial one.
    let staging = target
        .join(".rsi-tmp")
        .join(format!("target-pool-seed-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    fs::create_dir(&staging)?;
    let moved = (|| -> io::Result<(Value, Value)> {
        let pool_sha = chosen["sha"].as_str().unwrap_or_default();
        let times = normalise_mtimes(&git, &sandbox, pool_sha)?;
        let pool_target = PathBuf::from(chosen["target"].as_str().unwrap_or_default());
        let tree = seed_tree(&pool_target, &staging, wrapper_unlinks_outputs(env))?;
        for entry in fs::read_dir(&staging)?.flatten() {
            let destination = target.join(entry.file_name());
            if !destination.exists() {
                fs::rename(entry.path(), destination)?;
            }
        }
        Ok((tree, times))
    })();
    let _ = fs::remove_dir_all(&staging);
    drop(in_use);
    let (tree, times) = moved?;
    for (name, value) in [
        ("result", json!("seeded")),
        ("pool", chosen["name"].clone()),
        ("pool_sha", chosen["sha"].clone()),
        (
            "degraded",
            json!(chosen["degraded"].as_bool().unwrap_or(false)),
        ),
        ("distance", chosen["distance"].clone()),
    ] {
        outcome.insert(name.into(), value);
    }
    for part in [tree, times] {
        if let Value::Object(fields) = part {
            outcome.extend(fields);
        }
    }
    outcome.insert(
        "seconds".into(),
        json!((started.elapsed().as_secs_f64() * 10.0).round() / 10.0),
    );
    let outcome = Value::Object(outcome);
    fs::write(
        target.join(SEED_MARKER),
        format!("{}\n", dumps(&outcome, Some(1))),
    )?;
    Ok(outcome)
}

pub(crate) fn absolute(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// The outcome as printed: everything but the (potentially long) rebuilt units.
pub fn printed(outcome: &Value) -> String {
    let mut shown = outcome.clone();
    if let Some(map) = shown.as_object_mut() {
        map.remove("rebuilt_units");
    }
    dumps(&shown, None)
}

const USAGE: &str = "seed|build|seed-from|gc|reclaim-idle|refresh|status|summary|key [options]";

pub fn main(args: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some((command, rest)) = args.split_first() else {
        let _ = writeln!(err, "usage: target-pool {USAGE}");
        return 2;
    };
    let env = env_of(ctx);
    let printed = match command.as_str() {
        "seed" => Some(seed_command(rest, ctx, &env, err)),
        "key" => Some(key_command(rest, ctx, &env, err)),
        _ => None,
    };
    if let Some(result) = printed {
        return match result {
            Ok(Some(text)) => {
                let _ = writeln!(out, "{text}");
                0
            }
            Ok(None) => 2,
            Err(error) => {
                let _ = writeln!(err, "target-pool: {error}");
                1
            }
        };
    }
    let result = match command.as_str() {
        "build" => target_pool_cli::build(rest, ctx, &env, err),
        "seed-from" => target_pool_cli::seed_from(rest, ctx, &env, out, err),
        "gc" => target_pool_cli::gc(rest, ctx, &env, out, err),
        "reclaim-idle" => target_pool_cli::reclaim_idle(rest, ctx, &env, out, err),
        "refresh" => target_pool_cli::refresh(rest, ctx, &env, out, err),
        "status" => target_pool_cli::status(rest, ctx, &env, out, err),
        "summary" => target_pool_cli::summary(rest, ctx, &env, out, err),
        other => {
            let _ = writeln!(err, "target-pool: unknown command {other:?}");
            return 2;
        }
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "target-pool: {error}");
            1
        }
    }
}

fn seed_command(
    args: &[String],
    ctx: &Ctx,
    env: &Env,
    err: &mut dyn Write,
) -> io::Result<Option<String>> {
    let opts = [
        ("--sandbox", true),
        ("--target", true),
        ("--build-missing", false),
        ("--max-distance", true),
    ]
    .map(|(name, value)| cli::Opt { name, value });
    let parsed = match cli::parse(
        "target-pool seed",
        "--sandbox ROOT [--target DIR] [--build-missing] [--max-distance N]",
        args,
        &opts,
        &[],
        Some(0),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None),
    };
    let Some(sandbox) = parsed.value("--sandbox") else {
        let _ = writeln!(err, "target-pool: seed requires --sandbox");
        return Ok(None);
    };
    let max_distance = match parsed.value("--max-distance") {
        Some(text) => match text.parse() {
            Ok(n) => n,
            Err(_) => {
                let _ = writeln!(err, "target-pool: --max-distance must be an integer");
                return Ok(None);
            }
        },
        None => DEFAULT_MAX_DISTANCE,
    };
    let outcome = seed(
        &SeedOptions {
            sandbox: ctx.path(sandbox),
            target: parsed.value("--target").map(|t| ctx.path(t)),
            max_distance,
            build_missing: parsed.flag("--build-missing"),
            build_program: self_program()?,
        },
        env,
    )?;
    Ok(Some(printed(&outcome)))
}

/// The command prefix that runs `target-pool` again: this binary and the subcommand.
pub(crate) fn self_program() -> io::Result<Vec<String>> {
    Ok(vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "target-pool".to_string(),
    ])
}

fn key_command(
    args: &[String],
    ctx: &Ctx,
    env: &Env,
    err: &mut dyn Write,
) -> io::Result<Option<String>> {
    let opts = [cli::Opt {
        name: "--repo",
        value: true,
    }];
    let parsed = match cli::parse(
        "target-pool key",
        "[--repo DIR]",
        args,
        &opts,
        &[],
        Some(0),
        err,
    ) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None),
    };
    let repo = fs::canonicalize(ctx.path(parsed.value("--repo").unwrap_or(".")))?;
    Ok(Some(build_key(&repo, env)?))
}

#[cfg(test)]
#[path = "target_pool_tests.rs"]
mod tests;
