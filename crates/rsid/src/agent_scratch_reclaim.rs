//! Bounded reclaim of stale agent scratch outside sandboxes (#999, #932,
//! hardened by #1140).
//!
//! Agents, reviewers, tests and landers leave temp trees under `/var/tmp`
//! (disk-backed; `/tmp` is tmpfs) and worker `TMPDIR`s under
//! `~/.cache/rsi-*-tmp`. On 2026-09-28 32 GB of Sep 8-22 leftovers pushed the
//! hub below the 30 GB sandbox launch floor. Sandbox build-cache reclaim only
//! covers sandboxes, so this pass owns the rest.
//!
//! The principle is **when unsure, retain**. A top-level entry is deleted only
//! when ALL hold:
//!
//! - **Provenance.** Its record (`.rsi-scratch-record`) names an allocation in
//!   the private registry (`~/.rsi/scratch-registry`) that binds this very
//!   directory: kind, owner, device, inode, filesystem birth time and parent
//!   (see `registry.rs` for the trust boundary). Allocation
//!   ([`create_scratch_dir`], [`register_lander_owner`]) is the only way to
//!   get one, and only for a fresh, empty directory; adoption is impossible.
//!   Legacy directories, copies, moved trees and forged records are retained.
//! - **Confinement.** Its root was opened component by component with
//!   `O_NOFOLLOW`, every ancestor authenticated (root- or daemon-owned and
//!   closed to group/other writes, or sticky); the candidate sits on the
//!   root's device and mount (`statx` mount ids; a kernel that cannot report
//!   them retains) with no mount at or under it (`mountinfo`, including
//!   same-device binds). Identity is re-checked after enumeration, after the
//!   rename-aside and by every descriptor-relative step of the removal.
//! - **Age or ownership.** Nothing under it was written for the minimum age; a
//!   lander workspace whose registered owner is gone, interpreted in the PID
//!   namespace and boot it was recorded in (an owner from another namespace, or
//!   a malformed one, is ambiguous, so retained).
//! - **No holder.** A complete `/proc` proof (cwd, root, exe, fds, mappings,
//!   for every process and every thread not provably sharing them; any
//!   unexplained read error is "unproven") finds nothing holding it by path or
//!   inode. It runs again after the rename.
//! - **No lost work.** A complete walk finds every git repository below it
//!   (including under `target/`, `node_modules/` and inside a repository's own
//!   `.git`); each has a clean tree and nothing no remote-tracking ref has
//!   (local-only commits, tags, stash, detached HEAD). The one exemption is a
//!   lander's exact private clone: the directory the lander registered by inode
//!   before cloning into it, whose objects are borrowed (`alternates`). Any
//!   other repository, even inside a lander workspace, is checked in full.
//! - **Nothing changed.** After the rename the tree is walked again and its
//!   per-entry manifest (path, inode, type, size, mtime) must equal the proof's.
//!
//! The effectful pass renames the entry aside within its root, re-proves
//! everything under the rename, persists the proved manifest in the registry
//! (outside the tree), then deletes descriptor-relative, only entries the
//! manifest names, never crossing a device or mount or following a symlink; any
//! entry that is new, replaced or edited stops the deletion and retains the
//! rest. An interrupted removal resumes from the persisted manifest: the
//! survivors must all be in it, otherwise the tree is retained. Every phase,
//! including directory enumeration, draws on one time and entry budget; a dry
//! run reports the same decisions with no effects.

mod adopt;
mod build_targets;
mod cargo_targets;
mod fsys;
mod gitproof;
mod holders;
#[cfg(any(target_os = "macos", test))]
mod macos;
mod record;
mod registry;
#[cfg(all(test, target_os = "linux"))]
mod tests;

#[cfg(all(test, not(target_os = "linux")))]
mod unsupported_tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn scratch_without_linux_proofs_retains_files() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = temp.path().join("rsi-old-scratch");
        fs::create_dir(&scratch).unwrap();
        let work = scratch.join("work.txt");
        fs::write(&work, b"keep work").unwrap();
        let mut config =
            ScratchConfig::with_roots(vec![(temp.path().to_path_buf(), RootKind::VarTmp)]);
        config.min_age = Duration::ZERO;
        config.registry = temp.path().join("registry");
        let report = run(&config, SystemTime::now(), false);
        assert_eq!(report.reclaimed, 0);
        assert_eq!(fs::read(&work).unwrap(), b"keep work");
        let dir = record::pin_dir(&scratch).unwrap();
        assert!(dir.mount.is_none(), "unsupported mount proof stays unknown");
        assert_eq!(
            fsys::named_inventory(
                &dir,
                &mut Budget::new(
                    Instant::now() + config.max_duration,
                    config.max_scan_entries
                )
            )
            .err(),
            Some(fsys::InventoryError::Unproven)
        );
    }

    #[cfg(target_os = "macos")]
    fn macos_owner(pid: u32, ticks: u64, boot: &str) -> LanderOwner {
        let host = HostIdentity::current(Path::new("/proc")).unwrap();
        let host = HostIdentity {
            boot_id: boot.to_string(),
            ..host
        };
        let text = holders::owner_text(pid, ticks, &host, Path::new("/sandbox"));
        holders::lander_owner(&OwnerFile::Text(text), Path::new("/proc"))
    }

    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn macos_lander_owner_is_live_gone_or_ambiguous_by_start_time() {
        let pid = std::process::id();
        let ticks = lander_start_ticks().unwrap();
        assert_ne!(ticks, 0);
        let boot = macos::boot_time().unwrap();
        assert_eq!(macos_owner(pid, ticks, &boot), LanderOwner::Live);
        // A reused pid has another start time.
        assert_eq!(macos_owner(pid, ticks + 1, &boot), LanderOwner::Gone);
        // kern.boottime moves with clock steps: a stale value alone is not a reboot.
        assert_eq!(macos_owner(pid, ticks, "1.000000"), LanderOwner::Live);
        // A child that exited and was reaped no longer exists.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let child_pid = child.id();
        let child_ticks = match macos::probe(child_pid).unwrap() {
            macos::Probe::Started(micros) => micros,
            macos::Probe::Gone => ticks,
        };
        child.wait().unwrap();
        assert_eq!(macos::probe(child_pid).unwrap(), macos::Probe::Gone);
        assert_eq!(
            macos_owner(child_pid, child_ticks, &boot),
            LanderOwner::Gone
        );
        // An owner file from another PID namespace cannot be interpreted.
        let foreign = format!("v2 {pid} {ticks} pid:[4026531836] {boot} /sandbox\n");
        assert_eq!(
            holders::lander_owner(&OwnerFile::Text(foreign), Path::new("/proc")),
            LanderOwner::Ambiguous
        );
        // #252: a recorded pid now held by another user's process (root's
        // launchd, which libproc refuses this uid) is read exactly: its own
        // start time is that process, any other is a reused pid.
        let macos::Probe::Started(launchd) = macos::probe(1).expect("launchd start") else {
            panic!("launchd is running");
        };
        assert_eq!(macos_owner(1, launchd, &boot), LanderOwner::Live);
        assert_eq!(macos_owner(1, launchd + 1, &boot), LanderOwner::Gone);
    }

    // macOS keeps a birth time in `fstat` (#166): a fresh directory carries it,
    // so a scratch directory (and the manager's per-worktree build target) can
    // be registered. The mount proof stays unknown there.
    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn macos_fresh_directory_has_a_birth_time_and_registers() {
        // Canonical: the root is opened component by component with O_NOFOLLOW.
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let fresh = base.join("fresh");
        fs::create_dir(&fresh).unwrap();
        let pinned = record::pin_dir(&fresh).unwrap();
        let btime = pinned.btime.expect("APFS keeps a birth time");
        let now = i128::from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        ) * 1_000_000_000;
        assert!(
            (now - btime).abs() < 60 * 1_000_000_000,
            "birth time {btime} is not now ({now})"
        );
        assert!(pinned.mount.is_none());

        let registry = base.join("registry");
        let dir =
            record::create_scratch_dir_in(&registry, &base, "rsi-btime-macos", RootKind::VarTmp)
                .expect("a fresh directory registers once it has a birth time");
        assert!(dir.join(RECORD_FILE).is_file());

        let work = base.join("worktree");
        fs::create_dir(&work).unwrap();
        let target = cargo_targets::manager_target_in(&base.join("targets"), &registry, &work)
            .expect("manager separate-worktree build target is allocated");
        assert!(target.starts_with(base.join("targets")));
    }
}

use std::fs;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub use build_targets::build_targets_status;
use fsys::{Budget, Census, GitRoot, Ident, Pinned};
use gitproof::GitVerdict;
pub use holders::{Blocker, ProofFailure};
use holders::{HostIdentity, LanderOwner, OwnerFile};
use registry::{CloneBinding, Entry, ManifestState, Registry};

pub use adopt::{
    AdoptOutcome, AdoptRefusal, LegacyCandidate, LegacyListing, adopt_legacy, list_legacy,
};
pub(crate) use cargo_targets::{manager_target, seat_qa_target};
pub use record::{RECORD_FILE, create_scratch_dir, create_scratch_dir_in};

/// Conservative fixed default: three days.
pub const MIN_AGE: Duration = Duration::from_secs(72 * 3600);
pub const MAX_ENTRIES_PER_PASS: usize = 32;
pub const MAX_PASS_DURATION: Duration = Duration::from_secs(120);
/// Directory entries a whole pass may observe (enumeration, proofs, removal).
pub const MAX_SCAN_ENTRIES_PER_PASS: usize = 2_000_000;
/// Prefix of the rename-aside name; an interrupted pass leaves these behind
/// and a later pass finishes the delete.
const ASIDE_PREFIX: &str = ".rsi-reclaiming-";
/// Name prefix of a lander's private workspace (`tempfile` adds a random tail).
pub const LANDER_SCRATCH_PREFIX: &str = "rsi-rolling-land-";
/// File the lander writes in its workspace: `v2 <pid> <start_ticks> <pid_ns>
/// <boot_id> <sandbox>`. On macOS the start ticks are the process start time in
/// microseconds, the namespace is the fixed label `macos` and the boot id is
/// `kern.boottime` (see `macos.rs`).
pub const LANDER_OWNER_FILE: &str = ".rsi-lander-owner";
/// The directory name of a lander's private clone inside its workspace.
pub const LANDER_CLONE_DIR: &str = "repo";
/// A lander workspace with a record but no owner file (killed between creating
/// the directory and registering) is only reclaimed after this long.
pub const LANDER_UNREGISTERED_AGE: Duration = Duration::from_secs(60 * 60);
/// Entries listed in a report; counts stay exact beyond this.
const MAX_REPORTED_ENTRIES: usize = 64;
/// Blocking processes named across one report.
const MAX_REPORTED_BLOCKERS: usize = 64;
const OWNER_FILE_LIMIT: usize = 4096;
/// Landers' admin directories listed as roots, at most.
const MAX_LANDER_ROOTS: usize = 4096;
/// Tombstones examined per pass when pruning the registry (#1171).
const PRUNE_PER_PASS: usize = 256;

/// Which names a root lets the reclaim touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    /// `/var/tmp`: `rsi-*`, `dbcp-resume-*`, `ham-*`, 8-hex ids.
    VarTmp,
    /// `~/.cache`: worker TMPDIRs `rsi-*-tmp` only.
    WorkerCache,
    /// A lander workspace parent (the queue's cargo target directory, or a
    /// repository's `worktrees/<sandbox>` git admin directory):
    /// `rsi-rolling-land-*` private landing workspaces (#932). Liveness comes
    /// from the owner file the lander registers, not from age.
    LanderScratch,
    /// Registered manager targets and explicitly adopted legacy Cargo targets.
    CargoTarget,
    /// Dedicated legacy review-target container; every direct child is listed.
    CargoTargetPool,
}

impl RootKind {
    fn allows(self, name: &str) -> bool {
        match self {
            Self::VarTmp => {
                name.starts_with("rsi-")
                    || name.starts_with("dbcp-resume-")
                    || name.starts_with("ham-")
                    || (name.len() == 8
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
            }
            Self::WorkerCache => name.starts_with("rsi-") && name.ends_with("-tmp"),
            Self::LanderScratch => name.starts_with(LANDER_SCRATCH_PREFIX),
            Self::CargoTarget => cargo_targets::allows(name),
            Self::CargoTargetPool => true,
        }
    }

    /// The kind written into (and required of) an allocation.
    pub(crate) const fn tag(self) -> &'static str {
        match self {
            Self::VarTmp => "var_tmp",
            Self::WorkerCache => "worker_tmp",
            Self::LanderScratch => "lander",
            Self::CargoTarget | Self::CargoTargetPool => "cargo_target",
        }
    }
}

/// `.rsi-reclaiming-<original>-<pid>` -> `<original>`.
fn parse_aside(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(ASIDE_PREFIX)?;
    let (original, pid) = rest.rsplit_once('-')?;
    (!original.is_empty() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
        .then_some(original)
}

/// Whether `name` is a candidate for `kind`: an allowlisted name, or an aside
/// leftover whose original name was allowlisted.
fn name_allowed(kind: RootKind, name: &str) -> bool {
    kind.allows(name) || parse_aside(name).is_some_and(|original| kind.allows(original))
}

/// A job that is still queued or running: its id and working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenJob {
    pub id: String,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ScratchConfig {
    pub roots: Vec<(PathBuf, RootKind)>,
    pub min_age: Duration,
    /// Age gate for a recorded lander workspace with no owner file.
    pub unregistered_lander_age: Duration,
    pub max_entries: usize,
    pub max_duration: Duration,
    /// Directory entries the whole pass may observe.
    pub max_scan_entries: usize,
    /// Where to read the process inventory; `/proc` outside tests.
    pub proc_root: PathBuf,
    /// Where to read mounts; `/proc/self/mountinfo` outside tests.
    pub mountinfo: PathBuf,
    /// The allocation registry; `~/.rsi/scratch-registry` outside tests.
    pub registry: PathBuf,
    /// Drop the registry files of allocations a reclaim finished (tombstoned,
    /// #1171) at the end of a pass. On in production; a test may turn it off.
    pub registry_prune: bool,
    /// Deploy roots and installed symlinks are consumers even with no process.
    pub deploy_roots: Vec<PathBuf>,
    pub install_dirs: Vec<PathBuf>,
    /// The jobs still queued or running, so a build target whose source
    /// worktree is gone can be told from one a job is about to use (#646).
    /// `None` means unknown: the age floor then always applies.
    pub open_jobs: Option<Vec<OpenJob>>,
    /// Test seam: runs after a candidate was renamed aside, before the
    /// re-proof, to simulate a late writer.
    #[cfg(test)]
    pub after_rename: Option<fn(&Path)>,
    /// Test seam: runs after the final proof, before the first deletion.
    #[cfg(test)]
    pub after_proof: Option<fn(&Path)>,
    /// Test seam: runs after everything below was deleted, before the final
    /// removal of the (now empty) aside directory.
    #[cfg(test)]
    pub before_final_rmdir: Option<fn(&Path)>,
    /// Test seam: stop right after the tombstone is written, as an
    /// interruption would, leaving the registry removal to a prune.
    #[cfg(test)]
    pub stop_after_tombstone: bool,
    /// Test seam: runs after an adoption wrote its record, before the
    /// directory's timestamps are restored.
    #[cfg(test)]
    pub after_record_write: Option<fn(&Path)>,
}

impl ScratchConfig {
    fn with_roots(roots: Vec<(PathBuf, RootKind)>) -> Self {
        Self {
            roots,
            min_age: MIN_AGE,
            unregistered_lander_age: LANDER_UNREGISTERED_AGE,
            max_entries: MAX_ENTRIES_PER_PASS,
            max_duration: MAX_PASS_DURATION,
            max_scan_entries: MAX_SCAN_ENTRIES_PER_PASS,
            proc_root: PathBuf::from("/proc"),
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
            registry: registry::default_registry_path(),
            registry_prune: true,
            deploy_roots: Vec::new(),
            install_dirs: Vec::new(),
            open_jobs: None,
            #[cfg(test)]
            after_rename: None,
            #[cfg(test)]
            after_proof: None,
            #[cfg(test)]
            before_final_rmdir: None,
            #[cfg(test)]
            stop_after_tombstone: false,
            #[cfg(test)]
            after_record_write: None,
        }
    }

    /// `/var/tmp` and `$HOME/.cache` with the conservative defaults.
    #[must_use]
    pub fn standard() -> Self {
        let mut roots = vec![(PathBuf::from("/var/tmp"), RootKind::VarTmp)];
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            roots.push((PathBuf::from(home).join(".cache"), RootKind::WorkerCache));
        }
        roots.extend(lander_scratch_roots());
        roots.extend(cargo_targets::roots());
        let mut config = Self::with_roots(roots);
        config.deploy_roots = crate::deploy::DeployService::global()
            .allowed_roots()
            .to_vec();
        if let Some(home) = dirs::home_dir() {
            config
                .deploy_roots
                .push(home.join(".cargo/sat-target-deploy"));
            config.install_dirs = vec![home.join(".local/bin"), home.join(".rsi/install")];
        }
        config
    }
}

/// Where landers create private workspaces: the daemon queue's cargo target
/// (`~/.rsi/queue/target`) and, for hand-run landers with a sandbox-local
/// target, each repository `worktrees/<sandbox>` git admin directory under
/// `$HOME/rsi/.git` (#932). Listing only names the candidates (at most
/// [`MAX_LANDER_ROOTS`]); a root that is a symlink or has an inauthentic
/// ancestor is refused when the pass opens it.
#[must_use]
pub fn lander_scratch_roots() -> Vec<(PathBuf, RootKind)> {
    let mut roots = vec![(
        rsi_common::identity::data_dir()
            .join("queue")
            .join("target"),
        RootKind::LanderScratch,
    )];
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let admin = PathBuf::from(home).join("rsi/.git/worktrees");
        if let Ok(entries) = fs::read_dir(admin) {
            roots.extend(
                entries
                    .flatten()
                    .take(MAX_LANDER_ROOTS)
                    .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|entry| (entry.path(), RootKind::LanderScratch)),
            );
        }
    }
    roots
}

#[cfg(target_os = "macos")]
fn lander_start_ticks() -> io::Result<u64> {
    match macos::probe(std::process::id())? {
        macos::Probe::Started(micros) => Ok(micros),
        macos::Probe::Gone => Err(io::Error::other("cannot read lander start time")),
    }
}

#[cfg(not(target_os = "macos"))]
fn lander_start_ticks() -> io::Result<u64> {
    let pid = std::process::id();
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| crate::governor::parse_start_ticks(&stat))
        .filter(|ticks| *ticks != 0)
        .ok_or_else(|| io::Error::other("cannot read lander start time"))
}

/// Register the running lander as the owner of its freshly created, empty
/// private workspace: allocate it in the registry (kind `lander`, the sandbox
/// recorded as its source), write its record, then the owner file (pid, start
/// ticks, PID namespace, boot id) a sweep reads to tell a killed lander's
/// leftovers from a live one's. An error leaves the workspace unrecorded, which
/// retains it. Call [`register_lander_clone`] before cloning into it.
///
/// # Errors
/// The workspace is not fresh and empty, the filesystem keeps no birth time,
/// or the registry, record or owner file cannot be written.
pub fn register_lander_owner(workspace: &Path, sandbox: &Path) -> io::Result<()> {
    register_lander_owner_in(&registry::default_registry_path(), workspace, sandbox)
}

/// [`register_lander_owner`] against an explicit registry directory.
///
/// # Errors
/// As [`register_lander_owner`].
pub fn register_lander_owner_in(
    registry_path: &Path,
    workspace: &Path,
    sandbox: &Path,
) -> io::Result<()> {
    let ticks = lander_start_ticks()?;
    let host = HostIdentity::current(Path::new("/proc"))
        .ok_or_else(|| io::Error::other("cannot read PID namespace identity"))?;
    let (pinned, _entry) = record::allocate(
        registry_path,
        workspace,
        RootKind::LanderScratch,
        Some(sandbox),
    )?;
    fsys::create_file(
        &pinned,
        LANDER_OWNER_FILE,
        holders::owner_text(std::process::id(), ticks, &host, sandbox).as_bytes(),
    )
}

/// Register `workspace/repo` — a fresh, empty directory the lander just made —
/// as the lander's exact private clone: the only repository in the workspace
/// that is exempt from the unpublished-work check. Call it before
/// `git clone --shared --no-checkout` into the (existing, empty) directory.
///
/// # Errors
/// The workspace is not a registered lander workspace, the clone directory is
/// not fresh, empty and named [`LANDER_CLONE_DIR`], or the registry cannot be
/// updated.
pub fn register_lander_clone(workspace: &Path, clone: &Path) -> io::Result<()> {
    register_lander_clone_in(&registry::default_registry_path(), workspace, clone)
}

/// [`register_lander_clone`] against an explicit registry directory.
///
/// # Errors
/// As [`register_lander_clone`].
pub fn register_lander_clone_in(
    registry_path: &Path,
    workspace: &Path,
    clone: &Path,
) -> io::Result<()> {
    let uid = current_uid();
    if clone.parent() != Some(workspace)
        || clone.file_name().and_then(|n| n.to_str()) != Some(LANDER_CLONE_DIR)
    {
        return Err(io::Error::other("clone must be <workspace>/repo"));
    }
    let registry = Registry::open(registry_path, uid)?;
    let pinned = record::pin_dir(workspace)?;
    let record =
        record::read_record(&pinned).ok_or_else(|| io::Error::other("workspace has no record"))?;
    let mut entry = registry
        .get(&record.nonce)
        .ok_or_else(|| io::Error::other("workspace is not registered"))?;
    let parent = record::pin_dir(
        workspace
            .parent()
            .ok_or_else(|| io::Error::other("workspace has no parent"))?,
    )?;
    if !entry.binds(&pinned, &parent, RootKind::LanderScratch.tag(), uid) || entry.clone.is_some() {
        return Err(io::Error::other("workspace allocation does not bind"));
    }
    let clone_dir = record::pin_dir(clone)?;
    record::ensure_fresh(&clone_dir, uid)?;
    entry.clone = Some(CloneBinding {
        ident: clone_dir.ident,
        btime: clone_dir
            .btime
            .ok_or_else(|| io::Error::other("filesystem keeps no birth time"))?,
    });
    registry.replace(&entry)
}

/// One bounded sweep of a single lander workspace parent (lander startup).
pub fn sweep_lander_workspace(parent: &Path) -> ScratchReport {
    let mut config =
        ScratchConfig::with_roots(vec![(parent.to_path_buf(), RootKind::LanderScratch)]);
    // Startup must not stall a landing: a short sweep, finished by the daemon.
    config.max_duration = Duration::from_secs(20);
    run(&config, SystemTime::now(), false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Reclaimed (or would be, in a dry run).
    Reclaim,
    Young,
    Held,
    DirtyWorktree,
    /// Commits, tags, stashes or a detached HEAD that no remote-tracking ref
    /// has.
    Unpublished,
    /// No valid creation record: RSI cannot show it made this directory.
    Unrecorded,
    /// Not provable safe (inventory incomplete, git failed, mount or identity
    /// doubt, walk incomplete, root not authentic).
    Unproven,
    /// The tree changed between the proof and the rename (a late writer).
    Changed,
    /// Removal started but the pass budget ran out; a later pass finishes it.
    Partial,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct EntryReport {
    pub path: PathBuf,
    pub decision: Decision,
    pub bytes: u64,
    /// For an unproven candidate whose holder proof could not be completed:
    /// why, and the processes (kernel-reported, bounded) that blocked it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder_proof: Option<ProofFailure>,
}

/// Build targets one report names, at most (#3261); the totals stay exact.
const MAX_BUILD_TARGETS_REPORTED: usize = 64;
/// Build-target lines one logged pass writes, at most.
const MAX_BUILD_TARGET_LOG_LINES: usize = 16;

/// What one pass decided about one registered manager build target (#3261).
#[derive(Clone, Debug, Serialize)]
pub struct BuildTargetReport {
    /// The allocation's directory name under `build-targets`.
    pub name: String,
    pub decision: Decision,
    /// Why the decision stands: a closed token, see [`decision_reason`].
    pub reason: &'static str,
    /// Allocated bytes, when the walk fitted the pass budget.
    pub bytes: Option<u64>,
    /// Seconds since the newest write below it, when established.
    pub idle_secs: Option<u64>,
    /// The worktree the target was allocated for, when it was recorded (#646).
    pub worktree: Option<String>,
    /// An open job working in that worktree, when one is known (#646).
    pub job_id: Option<String>,
}

/// The closed reason token a decision is reported under.
#[must_use]
pub fn decision_reason(decision: Decision) -> &'static str {
    match decision {
        Decision::Reclaim => "reclaimable",
        Decision::Young => "idle_below_age_floor",
        Decision::Held => "held_by_consumer",
        Decision::DirtyWorktree => "dirty_git_worktree",
        Decision::Unpublished => "unpublished_git_history",
        Decision::Unrecorded => "unrecorded_allocation",
        Decision::Unproven => "not_provable_safe",
        Decision::Changed => "changed_during_proof",
        Decision::Partial => "removal_unfinished",
        Decision::Failed => "removal_failed",
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScratchReport {
    pub dry_run: bool,
    pub considered: u32,
    pub reclaimed: u32,
    pub reclaimed_bytes: u64,
    pub kept_young: u32,
    pub kept_held: u32,
    pub kept_dirty: u32,
    pub kept_unpublished: u32,
    pub kept_unrecorded: u32,
    pub kept_unproven: u32,
    pub kept_changed: u32,
    pub partial: u32,
    pub failed: u32,
    /// Roots refused before any entry was read (symlink, inauthentic
    /// ancestor, unreadable).
    pub refused_roots: u32,
    pub budget_exhausted: bool,
    pub entries: Vec<EntryReport>,
    /// Every registered build target the pass met (#3261), bounded.
    pub build_targets: Vec<BuildTargetReport>,
    /// Build targets met, including those past the bound.
    pub build_targets_total: u32,
    /// Blocking processes named in `entries` so far (capped per report).
    #[serde(skip)]
    blockers_named: usize,
}

impl ScratchReport {
    fn push_entry(
        &mut self,
        path: &Path,
        decision: Decision,
        bytes: u64,
        failure: Option<ProofFailure>,
    ) {
        if self.entries.len() >= MAX_REPORTED_ENTRIES {
            return;
        }
        // Only an unproven candidate carries blockers, and only up to the
        // report-wide cap; the rest are counted, not named.
        let holder_proof = failure
            .filter(|_| decision == Decision::Unproven)
            .map(|mut failure| {
                let room = MAX_REPORTED_BLOCKERS.saturating_sub(self.blockers_named);
                if failure.blockers.len() > room {
                    failure.omitted +=
                        u32::try_from(failure.blockers.len() - room).unwrap_or(u32::MAX);
                    failure.blockers.truncate(room);
                }
                self.blockers_named += failure.blockers.len();
                failure
            });
        self.entries.push(EntryReport {
            path: path.to_path_buf(),
            decision,
            bytes,
            holder_proof,
        });
    }

    fn record(&mut self, path: &Path, decision: Decision, bytes: u64) {
        self.record_with(path, decision, bytes, None);
    }

    fn record_with(
        &mut self,
        path: &Path,
        decision: Decision,
        bytes: u64,
        failure: Option<ProofFailure>,
    ) {
        match decision {
            Decision::Reclaim => {
                self.reclaimed += 1;
                self.reclaimed_bytes = self.reclaimed_bytes.saturating_add(bytes);
            }
            Decision::Young => self.kept_young += 1,
            Decision::Held => self.kept_held += 1,
            Decision::DirtyWorktree => self.kept_dirty += 1,
            Decision::Unpublished => self.kept_unpublished += 1,
            Decision::Unrecorded => self.kept_unrecorded += 1,
            Decision::Unproven => self.kept_unproven += 1,
            Decision::Changed => self.kept_changed += 1,
            Decision::Partial => self.partial += 1,
            Decision::Failed => self.failed += 1,
        }
        // Young entries are the common case; list only the interesting ones.
        if decision != Decision::Young {
            self.push_entry(path, decision, bytes, failure);
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    fn note_build_target(
        &mut self,
        name: &str,
        decision: Decision,
        bytes: Option<u64>,
        idle_ns: Option<i128>,
    ) {
        self.note_build_target_owned(name, decision, bytes, idle_ns, None);
    }

    fn note_build_target_owned(
        &mut self,
        name: &str,
        decision: Decision,
        bytes: Option<u64>,
        idle_ns: Option<i128>,
        owner: Option<cargo_targets::TargetOwner>,
    ) {
        self.build_targets_total += 1;
        if self.build_targets.len() >= MAX_BUILD_TARGETS_REPORTED {
            return;
        }
        self.build_targets.push(BuildTargetReport {
            name: name.to_string(),
            decision,
            reason: decision_reason(decision),
            bytes,
            idle_secs: idle_ns.map(|ns| u64::try_from(ns / 1_000_000_000).unwrap_or(0)),
            worktree: owner.as_ref().map(|owner| owner.worktree.clone()),
            job_id: owner.and_then(|owner| owner.job),
        });
    }

    /// Whether `path` is a child of the manager build-targets root.
    fn is_build_target(&self, path: &Path) -> bool {
        path.parent()
            .is_some_and(cargo_targets::is_build_targets_root)
    }

    fn refuse_root(&mut self, root: &Path) {
        self.refused_roots += 1;
        self.push_entry(root, Decision::Unproven, 0, None);
    }
}

static LAST_REPORT: Mutex<Option<ScratchReport>> = Mutex::new(None);

/// The most recent pass (dry or effectful) in this daemon process.
#[must_use]
pub fn last_report() -> Option<ScratchReport> {
    LAST_REPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// One bounded line per registered build target the pass met, with its reason
/// (#3261): a manager can see why the space stays pinned. Reclaimed targets are
/// named too, so a freed target is traceable to the pass that removed it.
fn log_build_targets(trigger: &str, report: &ScratchReport) {
    for target in report.build_targets.iter().take(MAX_BUILD_TARGET_LOG_LINES) {
        tracing::info!(
            trigger,
            dry_run = report.dry_run,
            target = %target.name,
            decision = ?target.decision,
            reason = target.reason,
            bytes = target.bytes,
            idle_secs = target.idle_secs,
            "Agent scratch reclaim build target decision"
        );
    }
    let logged = u32::try_from(report.build_targets.len().min(MAX_BUILD_TARGET_LOG_LINES))
        .unwrap_or(u32::MAX);
    let unlogged = report.build_targets_total.saturating_sub(logged);
    if unlogged > 0 {
        tracing::info!(
            trigger,
            build_targets_total = report.build_targets_total,
            unlogged,
            "Agent scratch reclaim build target decisions truncated"
        );
    }
}

/// The jobs still queued or running, for [`ScratchConfig::open_jobs`]. `None`
/// when the store cannot say, which keeps the age floor on every build target.
#[must_use]
pub fn open_jobs(store: &rsid_store::store::Store) -> Option<Vec<OpenJob>> {
    let rows = store.list_open_agent_jobs().ok()?;
    Some(
        rows.into_iter()
            .map(|row| OpenJob {
                id: row.job.id.to_string(),
                cwd: PathBuf::from(row.job.cwd),
            })
            .collect(),
    )
}

/// Run one pass with the standard config and log what it freed. The age floor
/// stays on every build target: the open-job list is unknown.
pub fn run_and_log(dry_run: bool, trigger: &str) -> ScratchReport {
    run_and_log_with_jobs(dry_run, trigger, None)
}

/// [`run_and_log`] knowing the open jobs, so a build target whose worktree is
/// gone and that no open job names is reclaimed at once (#646).
pub fn run_and_log_with_jobs(
    dry_run: bool,
    trigger: &str,
    open_jobs: Option<Vec<OpenJob>>,
) -> ScratchReport {
    let mut config = ScratchConfig::standard();
    config.open_jobs = open_jobs;
    let report = run(&config, SystemTime::now(), dry_run);
    if report.reclaimed > 0
        || report.failed > 0
        || report.kept_dirty > 0
        || report.kept_unpublished > 0
        || report.kept_unrecorded > 0
        || report.kept_unproven > 0
        || report.refused_roots > 0
        || report.partial > 0
    {
        tracing::info!(
            trigger,
            dry_run = report.dry_run,
            considered = report.considered,
            reclaimed = report.reclaimed,
            reclaimed_bytes = report.reclaimed_bytes,
            kept_held = report.kept_held,
            kept_dirty = report.kept_dirty,
            kept_unpublished = report.kept_unpublished,
            kept_unrecorded = report.kept_unrecorded,
            kept_unproven = report.kept_unproven,
            kept_changed = report.kept_changed,
            partial = report.partial,
            failed = report.failed,
            refused_roots = report.refused_roots,
            budget_exhausted = report.budget_exhausted,
            "Agent scratch reclaim pass completed"
        );
    }
    log_build_targets(trigger, &report);
    // A candidate kept for lack of provenance or proof stays on disk until an
    // operator acts (#2488): name the first few, never silently. A registered
    // build target has its own line above and never competes for these slots.
    for entry in report
        .entries
        .iter()
        .filter(|entry| matches!(entry.decision, Decision::Unrecorded | Decision::Unproven))
        .filter(|entry| !report.is_build_target(&entry.path))
        .take(8)
    {
        tracing::info!(
            trigger,
            path = %entry.path.display(),
            decision = ?entry.decision,
            bytes = entry.bytes,
            "Agent scratch reclaim kept a candidate it could not prove"
        );
    }
    // The processes that kept candidates unproven, for the operator (the same
    // detail is in `last_report()` and the health status).
    if let Some(blockers) = report
        .entries
        .iter()
        .find_map(|entry| entry.holder_proof.as_ref())
    {
        for blocker in &blockers.blockers {
            tracing::debug!(summary = %blocker.summary, "Agent scratch reclaim blocked by process");
        }
    }
    *LAST_REPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report.clone());
    report
}

fn current_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn unix_ns(time: SystemTime) -> i128 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => i128::try_from(d.as_nanos()).unwrap_or(i128::MAX),
        Err(e) => -i128::try_from(e.duration().as_nanos()).unwrap_or(i128::MAX),
    }
}

/// A top-level entry found in a root.
struct Candidate {
    name: String,
    ident: Ident,
    mtime_ns: i128,
}

/// What a candidate proved to be, ready to reclaim.
struct Prepared {
    pinned: Pinned,
    census: Census,
    aside: bool,
    entry: Entry,
    /// The manifest persisted by an earlier pass's final proof (a resumed
    /// removal): survivors must all be in it.
    resume: Option<Vec<u64>>,
}

/// How `assess` establishes that RSI made the candidate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Provenance {
    /// The reclaim rule: a creation record bound by the registry.
    Recorded,
    /// Operator adoption (#1147): the legacy directory has no record. Every
    /// other proof still runs; an in-memory allocation stands in for the
    /// registry entry the adoption would create.
    Legacy,
}

enum Assessment {
    /// Not a candidate (vanished, wrong type/owner).
    Skip,
    /// [`Provenance::Legacy`] only: the directory already has a bound record.
    AlreadyRecorded,
    Keep(Decision),
    Reclaim(Box<Prepared>),
}

/// State of one pass.
struct Pass<'a> {
    config: &'a ScratchConfig,
    now_ns: i128,
    uid: u32,
    budget: Budget,
    mounts: Option<Vec<PathBuf>>,
    registry: Option<Registry>,
    /// `None` until first needed; inner `None` is an unprovable inventory.
    holders: Option<Result<holders::Holders, ProofFailure>>,
    /// Why the last holder proof failed, for the candidate being assessed.
    /// Taken by the run loop after each candidate.
    failure: Option<ProofFailure>,
    /// How long the candidate being assessed has been quiet, when the age was
    /// established (#3261). Taken by the run loop after each candidate.
    idle_ns: Option<i128>,
    /// The source worktree and open job of the build target being assessed
    /// (#646). Taken by the run loop after each candidate.
    owner: Option<cargo_targets::TargetOwner>,
}

/// Whether `git` is the lander's exact registered private clone, still
/// borrowing objects from the source (`alternates`): the one repository whose
/// unpublished merge commits and missing checkout are by design.
fn is_registered_clone(pinned: &Pinned, entry: &Entry, git: &GitRoot) -> bool {
    let Some(clone) = entry.clone else {
        return false;
    };
    // The exact directory the lander registered: same inode AND same birth
    // time, so a different clone that reused the inode does not inherit it.
    if git.bare
        || clone.ident != git.ident
        || git.btime != Some(clone.btime)
        || git.rel != Path::new(LANDER_CLONE_DIR)
    {
        return false;
    }
    let alternates = fsys::read_small_file_below(
        pinned,
        &[LANDER_CLONE_DIR, ".git", "objects", "info", "alternates"],
        OWNER_FILE_LIMIT,
    );
    matches!(alternates, Ok(Some(text))
    if text.lines().any(|l| !l.trim().is_empty())
        && text.lines().filter(|l| !l.trim().is_empty()).all(|l| {
            Path::new(l.trim()).is_absolute() && l.trim().ends_with("/objects")
        }))
}

/// Whether `git` is a linked worktree the lander made of its registered private
/// clone (`git worktree add`: the `candidate-*` and `base-*` checkouts, #2488).
/// Its `.git` file must name an admin directory directly under
/// `<workspace>/repo/.git/worktrees/`, by the workspace's registered path (the
/// text survives the reclaim's rename-aside). Such a checkout owns no objects or
/// refs of its own: its HEAD is the lander's detached integration merge, which
/// is unpublished by design, exactly like the clone it borrows from. The gitfile
/// may outlive a clone an interrupted reclaim already removed; that is the same
/// private state. A gitfile naming anywhere else is a foreign repository and
/// gets the full checks.
fn is_registered_clone_worktree(pinned: &Pinned, entry: &Entry, git: &GitRoot) -> bool {
    if entry.clone.is_none() || git.bare || git.rel.as_os_str().is_empty() {
        return false;
    }
    let mut components: Vec<&str> = Vec::new();
    for component in git.rel.components() {
        match component {
            std::path::Component::Normal(name) => match name.to_str() {
                Some(name) => components.push(name),
                None => return false,
            },
            _ => return false,
        }
    }
    components.push(".git");
    let Ok(Some(text)) = fsys::read_small_file_below(pinned, &components, OWNER_FILE_LIMIT) else {
        return false;
    };
    let Some(target) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
    else {
        return false;
    };
    let admin = Path::new(&entry.path)
        .join(LANDER_CLONE_DIR)
        .join(".git")
        .join("worktrees");
    let Ok(name) = Path::new(target).strip_prefix(&admin) else {
        return false;
    };
    let mut parts = name.components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// A lander's workspace is private: its registered clone and the linked
/// worktrees it made of that clone are exempt; a Cargo target's test-fixture
/// repositories are exempt too (#3261); everything else is checked.
fn git_exemption<'e>(
    kind: RootKind,
    pinned: &'e Pinned,
    entry: &'e Entry,
) -> impl Fn(&GitRoot) -> bool + 'e {
    move |git| match kind {
        RootKind::LanderScratch => {
            is_registered_clone(pinned, entry, git)
                || is_registered_clone_worktree(pinned, entry, git)
        }
        RootKind::CargoTarget => cargo_targets::is_fixture_repo(git),
        _ => false,
    }
}

impl<'a> Pass<'a> {
    fn new(config: &'a ScratchConfig, now: SystemTime, started: Instant, uid: u32) -> Self {
        Self {
            config,
            now_ns: unix_ns(now),
            uid,
            budget: Budget::new(started + config.max_duration, config.max_scan_entries),
            mounts: fsys::read_mounts(&config.mountinfo),
            registry: Registry::open(&config.registry, uid).ok(),
            holders: None,
            failure: None,
            idle_ns: None,
            owner: None,
        }
    }

    /// Records the owner of a registered build target for the report, and
    /// whether it is orphaned: its source worktree is gone, the open-job list is
    /// known and no open job works there (#646). Anything unknown is not orphaned.
    fn is_orphaned_build_target(&mut self, root_path: &Path, entry: &Entry) -> bool {
        if !cargo_targets::is_build_targets_root(root_path) {
            return false;
        }
        let owner = cargo_targets::owner_of(entry, self.config.open_jobs.as_deref());
        let orphaned = owner.as_ref().is_some_and(|owner| owner.orphaned);
        self.owner = owner;
        orphaned
    }

    fn holders(&mut self, own_fds: &[RawFd]) -> Option<&holders::Holders> {
        if self.holders.is_none() {
            self.holders = Some(holders::scan(
                &self.config.proc_root,
                self.uid,
                own_fds,
                &mut self.budget,
            ));
        }
        match self.holders.as_ref() {
            Some(Ok(holders)) => Some(holders),
            Some(Err(failure)) => {
                self.failure = Some(failure.clone());
                None
            }
            None => None,
        }
    }

    /// Top-level entries of `root` that pass the name policy, oldest first.
    /// `Err` when the listing could not be read within the budget.
    fn enumerate(&mut self, root: &Pinned, kind: RootKind) -> io::Result<Vec<Candidate>> {
        let names = fsys::entry_names(root.raw(), true, &mut self.budget)?;
        let mut candidates = Vec::new();
        for name in names {
            // Name policy first: never stat what we would not touch.
            if !name_allowed(kind, &name) {
                continue;
            }
            let Ok(stat) = fsys::stat_at(root.raw(), &name) else {
                continue;
            };
            #[allow(clippy::unnecessary_cast)]
            if !fsys::is_dir(&stat) || stat.st_uid as u32 != self.uid {
                continue;
            }
            candidates.push(Candidate {
                ident: fsys::ident_of(&stat),
                name,
                mtime_ns: fsys::mtime_ns(&stat),
            });
        }
        // Oldest first: the pass frees the stalest data and yields.
        candidates.sort_by_key(|c| c.mtime_ns);
        Ok(candidates)
    }

    /// The registry entry that binds `pinned` as a directory of `kind` made in
    /// `root`, via the nonce in its record.
    fn allocation(&self, pinned: &Pinned, root: &Pinned, kind: RootKind) -> Option<Entry> {
        let rec = record::read_record(pinned)?;
        if rec.kind != kind.tag() {
            return None;
        }
        let entry = self.registry.as_ref()?.get(&rec.nonce)?;
        entry
            .binds(pinned, root, kind.tag(), self.uid)
            .then_some(entry)
    }

    /// The lander owner state of `pinned`.
    fn lander_owner(&self, pinned: &Pinned) -> LanderOwner {
        let file = match fsys::read_small_file(pinned, LANDER_OWNER_FILE, OWNER_FILE_LIMIT) {
            Ok(None) => OwnerFile::Absent,
            Ok(Some(text)) => OwnerFile::Text(text),
            Err(_) => OwnerFile::Unreadable,
        };
        holders::lander_owner(&file, &self.config.proc_root)
    }

    #[allow(clippy::too_many_lines)] // One ordered proof; splitting hides the order.
    fn assess(
        &mut self,
        root: &Pinned,
        root_path: &Path,
        kind: RootKind,
        cand: &Candidate,
        provenance: Provenance,
    ) -> Assessment {
        let path = root_path.join(&cand.name);
        let aside = parse_aside(&cand.name).is_some();
        let pinned = match fsys::pin_child(root, &cand.name) {
            Ok(pinned) => pinned,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Assessment::Skip,
            Err(_) => return Assessment::Keep(Decision::Unproven),
        };
        // Replaced between enumeration and now, or foreign-owned.
        if pinned.ident != cand.ident || pinned.uid != self.uid {
            return Assessment::Keep(Decision::Unproven);
        }
        if !fsys::root_unchanged(root_path, self.uid, root)
            || !unmounted(root, &pinned, &path, self.mounts.as_deref())
        {
            return Assessment::Keep(Decision::Unproven);
        }
        let entry = match provenance {
            Provenance::Recorded => {
                let Some(entry) = self.allocation(&pinned, root, kind) else {
                    return Assessment::Keep(Decision::Unrecorded);
                };
                entry
            }
            Provenance::Legacy => {
                if self.allocation(&pinned, root, kind).is_some() {
                    return Assessment::AlreadyRecorded;
                }
                // An aside leftover is mid-removal, not legacy scratch.
                let Some(entry) = (!aside)
                    .then(|| record::legacy_entry(&pinned, root, kind, &path, self.uid))
                    .flatten()
                else {
                    return Assessment::Keep(Decision::Unproven);
                };
                entry
            }
        };
        // Name the owning worktree and job for the report whatever the verdict
        // (#646); an orphaned target skips the age floor below.
        let orphaned = self.is_orphaned_build_target(root_path, &entry);
        match cargo_targets::has_consumer(&path, &entry.path, self.config, &mut self.budget) {
            Ok(true) => return Assessment::Keep(Decision::Held),
            Err(_) => return Assessment::Keep(Decision::Unproven),
            Ok(false) => {}
        }
        // A Cargo target is cache only. Never move manager state or unknown
        // content, even temporarily, and apply this guard to legacy adoption.
        if matches!(kind, RootKind::CargoTarget | RootKind::CargoTargetPool)
            && !cargo_targets::cache_only(&pinned, &mut self.budget)
        {
            return Assessment::Keep(Decision::Unproven);
        }
        // An earlier pass's final proof, if this is a resumed removal. A proof
        // that exists but cannot be used is not "no proof": retain.
        let resume = if aside {
            match self
                .registry
                .as_ref()
                .map_or(ManifestState::Invalid, |registry| {
                    registry.manifest_get(&entry.nonce)
                }) {
                ManifestState::Absent => None,
                ManifestState::Present(stored) => Some(stored),
                ManifestState::Invalid => return Assessment::Keep(Decision::Unproven),
            }
        } else {
            None
        };

        // The age the whole tree must have been quiet for, when one applies.
        let mut age_gate = None;
        if kind == RootKind::LanderScratch {
            match self.lander_owner(&pinned) {
                LanderOwner::Live => return Assessment::Keep(Decision::Held),
                LanderOwner::Ambiguous => return Assessment::Keep(Decision::Unproven),
                // The owner is gone: nothing left to protect, no age gate.
                LanderOwner::Gone => {}
                LanderOwner::Unregistered => {
                    if resume.is_none() {
                        age_gate = Some(self.config.unregistered_lander_age);
                    }
                }
            }
        } else if resume.is_none() {
            // A build target whose source worktree is gone and that no open
            // job names has nothing left to wait for (#646): no age floor.
            // Every other proof below still runs.
            age_gate = (!orphaned).then_some(self.config.min_age);
        }
        if let Some(gate) = age_gate
            && let Some(keep) = self.too_young(&pinned, gate)
        {
            return Assessment::Keep(keep);
        }
        if self.budget.is_exhausted() {
            return Assessment::Keep(Decision::Unproven);
        }

        let Some(census) = fsys::census(&pinned, &mut self.budget) else {
            return Assessment::Keep(Decision::Unproven);
        };
        if let Some(stored) = &resume {
            // A resumed removal: everything that survived must be something the
            // earlier final proof saw, unchanged.
            if !census
                .manifest
                .iter()
                .all(|h| fsys::manifest_contains(stored, *h))
            {
                return Assessment::Keep(Decision::Changed);
            }
        } else if let Some(gate) = age_gate {
            // Quiet means quiet everywhere: a deep recent write keeps it young.
            let age = self.now_ns.saturating_sub(census.newest_mtime_ns).max(0);
            self.idle_ns = Some(age);
            if age < i128::try_from(gate.as_nanos()).unwrap_or(i128::MAX) {
                return Assessment::Keep(Decision::Young);
            }
        }
        let Some(holders) = self.holders(&[root.raw(), pinned.raw()]) else {
            return Assessment::Keep(Decision::Unproven);
        };
        if holders.holds(&path, Some(&census.idents)) {
            return Assessment::Keep(Decision::Held);
        }
        if resume.is_none() {
            let skip = git_exemption(kind, &pinned, &entry);
            match gitproof::check_all(&fd_anchor(&pinned), &census.gits, &skip, &mut self.budget) {
                GitVerdict::Clean => {}
                GitVerdict::Dirty => return Assessment::Keep(Decision::DirtyWorktree),
                GitVerdict::Unpublished => return Assessment::Keep(Decision::Unpublished),
                GitVerdict::Unproven => return Assessment::Keep(Decision::Unproven),
            }
        }
        Assessment::Reclaim(Box::new(Prepared {
            pinned,
            census,
            aside,
            entry,
            resume,
        }))
    }

    /// Allocated bytes of a kept candidate, by one more bounded walk (#3261);
    /// `None` when the budget no longer allows it. Also fills the idle age when
    /// the proof stopped before establishing it.
    fn measure(&mut self, root: &Pinned, cand: &Candidate) -> Option<u64> {
        if self.budget.is_exhausted() {
            return None;
        }
        let pinned = fsys::pin_child(root, &cand.name).ok()?;
        if pinned.ident != cand.ident {
            return None;
        }
        let census = fsys::census(&pinned, &mut self.budget)?;
        if self.idle_ns.is_none() {
            self.idle_ns = Some(self.now_ns.saturating_sub(census.newest_mtime_ns).max(0));
        }
        Some(census.bytes)
    }

    /// `Some(reason)` to keep: too young, or the age could not be established
    /// within the budget.
    fn too_young(&mut self, pinned: &Pinned, min_age: Duration) -> Option<Decision> {
        let Some(newest) = fsys::newest_mtime_ns(pinned, &mut self.budget) else {
            return Some(Decision::Unproven);
        };
        let age = self.now_ns.saturating_sub(newest).max(0);
        self.idle_ns = Some(age);
        (age < i128::try_from(min_age.as_nanos()).unwrap_or(i128::MAX)).then_some(Decision::Young)
    }

    /// Rename aside, re-prove everything under the rename, then delete only
    /// what the proof names.
    fn reclaim(
        &mut self,
        root: &Pinned,
        root_path: &Path,
        kind: RootKind,
        cand: &Candidate,
        prepared: &Prepared,
    ) -> io::Result<Decision> {
        // Serialize with registry pruning from the rename aside to the removal
        // of the allocation's entry and manifest.
        let Some(_custody) = self
            .registry
            .as_ref()
            .ok_or_else(|| io::Error::other("registry unavailable"))?
            .reclaim_guard()?
        else {
            // A prune holds the registry: defer to a later pass, never wait.
            return Ok(Decision::Held);
        };
        let aside_name = if prepared.aside {
            cand.name.clone()
        } else {
            format!("{ASIDE_PREFIX}{}-{}", cand.name, std::process::id())
        };
        if !prepared.aside {
            fsys::rename_noreplace(root, &cand.name, &aside_name)?;
        }
        #[cfg(test)]
        if let Some(hook) = self.config.after_rename {
            hook(&root_path.join(&aside_name));
        }
        let decision = self.reprove(root, root_path, kind, &aside_name, prepared);
        if decision != Decision::Reclaim {
            if !prepared.aside {
                // Put it back; if that fails the aside name is finished by a
                // later pass only if it proves out again.
                fsys::rename_noreplace(root, &aside_name, &cand.name)?;
            }
            return Ok(decision);
        }
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| io::Error::other("registry unavailable"))?;
        // The manifest the deletion stays inside lives outside the tree. A
        // resumed removal keeps its original; a fresh one persists the proof.
        let manifest: &[u64] = match &prepared.resume {
            Some(stored) => stored,
            None => {
                registry.manifest_put(&prepared.entry.nonce, &prepared.census.manifest)?;
                &prepared.census.manifest
            }
        };
        #[cfg(test)]
        if let Some(hook) = self.config.after_proof {
            hook(&root_path.join(&aside_name));
        }
        let keep = [RECORD_FILE, LANDER_OWNER_FILE];
        match fsys::remove_contents(&prepared.pinned, &keep, manifest, &mut self.budget) {
            fsys::Removal::Done => {}
            fsys::Removal::OutOfBudget => return Ok(Decision::Partial),
            fsys::Removal::Drift => {
                // Something was added or changed: drop the manifest so the
                // remainder must prove itself in full next time.
                registry.manifest_remove(&prepared.entry.nonce);
                return Ok(Decision::Changed);
            }
            fsys::Removal::Stopped => return Ok(Decision::Failed),
        }
        // Provenance goes last so an interrupted finish is still recognised.
        for name in keep.iter().rev() {
            if fsys::unlink_proved(&prepared.pinned, name, manifest)? == fsys::Removal::Drift {
                registry.manifest_remove(&prepared.entry.nonce);
                return Ok(Decision::Changed);
            }
        }
        // The deletions above must be durable before anything concludes from
        // them; a sync that fails stops here, with no tombstone (#1171).
        if fsys::sync_dir(&prepared.pinned).is_err() {
            return Ok(Decision::Failed);
        }
        #[cfg(test)]
        if let Some(hook) = self.config.before_final_rmdir {
            hook(&root_path.join(&aside_name));
        }
        // Only the directory that was proved: not whatever now has its name. A
        // directory that is not removed (late contents, a swapped name) is
        // retained and never tombstoned.
        if fsys::rmdir_proved(root, &aside_name, prepared.pinned.ident)? == fsys::Removal::Drift {
            registry.manifest_remove(&prepared.entry.nonce);
            return Ok(Decision::Changed);
        }
        // The name was removed; was it the allocation? A concurrent rename can
        // leave a different directory at the name for that rmdir to remove,
        // while the allocation lives on elsewhere. Only an inode with no links
        // left is proved removed; anything else (or a filesystem that does not
        // say so) is retained with its entry and never tombstoned.
        if !fsys::is_unlinked(prepared.pinned.raw()) {
            registry.manifest_remove(&prepared.entry.nonce);
            return Ok(Decision::Changed);
        }
        if fsys::sync_dir(root).is_err() {
            return Ok(Decision::Failed);
        }
        // The directory is durably gone, so is anything that could name this
        // allocation's nonce: record that before the last registry steps, so an
        // interruption after this point is finished by a prune (#1171). A
        // failed write leaves the plain cleanup below, which is also safe.
        let _ = registry.tombstone_put(&prepared.entry);
        #[cfg(test)]
        if self.config.stop_after_tombstone {
            return Ok(Decision::Reclaim);
        }
        registry.finish_reclaimed(&prepared.entry.nonce);
        Ok(Decision::Reclaim)
    }

    /// The final proof, run on the renamed tree: identity, mounts, allocation,
    /// owner, unchanged contents, holders and git. `Reclaim` means all hold.
    fn reprove(
        &mut self,
        root: &Pinned,
        root_path: &Path,
        kind: RootKind,
        aside_name: &str,
        prepared: &Prepared,
    ) -> Decision {
        let aside_path = root_path.join(aside_name);
        let pinned = &prepared.pinned;
        match fsys::pin_child(root, aside_name) {
            Ok(again) if again.ident == pinned.ident => {}
            _ => return Decision::Unproven,
        }
        let mounts = fsys::read_mounts(&self.config.mountinfo);
        if !fsys::root_unchanged(root_path, self.uid, root)
            || !unmounted(root, pinned, &aside_path, mounts.as_deref())
        {
            return Decision::Unproven;
        }
        match cargo_targets::has_consumer(
            &aside_path,
            &prepared.entry.path,
            self.config,
            &mut self.budget,
        ) {
            Ok(true) => return Decision::Held,
            Err(_) => return Decision::Unproven,
            Ok(false) => {}
        }
        // The allocation must still bind this exact directory.
        if self.allocation(pinned, root, kind).as_ref() != Some(&prepared.entry) {
            return Decision::Unrecorded;
        }
        if kind == RootKind::LanderScratch {
            match self.lander_owner(pinned) {
                LanderOwner::Live => return Decision::Held,
                LanderOwner::Ambiguous => return Decision::Unproven,
                LanderOwner::Gone | LanderOwner::Unregistered => {}
            }
        }
        let Some(census) = fsys::census(pinned, &mut self.budget) else {
            return Decision::Unproven;
        };
        match &prepared.resume {
            // A resumed removal is a shrinking tree: survivors must be known.
            Some(stored) => {
                if !census
                    .manifest
                    .iter()
                    .all(|h| fsys::manifest_contains(stored, *h))
                {
                    return Decision::Changed;
                }
            }
            // A fresh one must be exactly what was proved.
            None => {
                if census.manifest != prepared.census.manifest {
                    return Decision::Changed;
                }
            }
        }
        // A fresh inventory: nothing from the first proof is reused.
        let holders = match holders::scan(
            &self.config.proc_root,
            self.uid,
            &[root.raw(), pinned.raw()],
            &mut self.budget,
        ) {
            Ok(holders) => holders,
            Err(failure) => {
                self.failure = Some(failure);
                return Decision::Unproven;
            }
        };
        if holders.holds(&aside_path, Some(&census.idents)) {
            return Decision::Held;
        }
        if prepared.resume.is_none() {
            let skip = git_exemption(kind, pinned, &prepared.entry);
            match gitproof::check_all(&fd_anchor(pinned), &census.gits, &skip, &mut self.budget) {
                GitVerdict::Clean => {}
                GitVerdict::Dirty => return Decision::DirtyWorktree,
                GitVerdict::Unpublished => return Decision::Unpublished,
                GitVerdict::Unproven => return Decision::Unproven,
            }
        }
        if self.budget.is_exhausted() {
            return Decision::Unproven;
        }
        Decision::Reclaim
    }
}

/// Whether `path`, at identity `pinned`, is on the root's device and mount
/// with no mount at or under it.
fn unmounted(root: &Pinned, pinned: &Pinned, path: &Path, mounts: Option<&[PathBuf]>) -> bool {
    fsys::on_root_mount(root, pinned)
        && mounts.is_some_and(|mounts| !fsys::mount_at_or_under(mounts, path))
}

/// An address for `pinned` that cannot be redirected: the descriptor itself,
/// as seen through `/proc`.
fn fd_anchor(pinned: &Pinned) -> PathBuf {
    PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), pinned.raw()))
}

/// Run one bounded pass over the configured roots.
pub fn run(config: &ScratchConfig, now: SystemTime, dry_run: bool) -> ScratchReport {
    let started = Instant::now();
    let uid = current_uid();
    let mut pass = Pass::new(config, now, started, uid);
    let mut report = ScratchReport {
        dry_run,
        ..ScratchReport::default()
    };
    let mut effects = 0usize;
    'roots: for (root_path, kind) in &config.roots {
        let root = match fsys::open_root(root_path, uid) {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                report.refuse_root(root_path);
                continue;
            }
        };
        let build_targets = cargo_targets::is_build_targets_root(root_path);
        let candidates = match pass.enumerate(&root, *kind) {
            Ok(candidates) => candidates,
            Err(_) if pass.budget.is_exhausted() => {
                report.budget_exhausted = true;
                break 'roots;
            }
            Err(_) => {
                report.refuse_root(root_path);
                continue;
            }
        };
        for cand in candidates {
            if pass.budget.expired() || effects >= config.max_entries {
                report.budget_exhausted = true;
                break 'roots;
            }
            let path = root_path.join(&cand.name);
            pass.failure = None;
            pass.idle_ns = None;
            pass.owner = None;
            let build_target = *kind == RootKind::CargoTarget && build_targets;
            match pass.assess(&root, root_path, *kind, &cand, Provenance::Recorded) {
                Assessment::Skip | Assessment::AlreadyRecorded => {}
                Assessment::Keep(decision) => {
                    report.considered += 1;
                    if build_target {
                        let bytes = pass.measure(&root, &cand);
                        report.note_build_target_owned(
                            &cand.name,
                            decision,
                            bytes,
                            pass.idle_ns,
                            pass.owner.take(),
                        );
                    }
                    report.record_with(&path, decision, 0, pass.failure.take());
                }
                Assessment::Reclaim(prepared) => {
                    report.considered += 1;
                    effects += 1;
                    let bytes = prepared.census.bytes;
                    let idle_ns = pass.idle_ns;
                    if dry_run {
                        if build_target {
                            report.note_build_target_owned(
                                &cand.name,
                                Decision::Reclaim,
                                Some(bytes),
                                idle_ns,
                                pass.owner.take(),
                            );
                        }
                        report.record(&path, Decision::Reclaim, bytes);
                        continue;
                    }
                    let outcome = match pass.reclaim(&root, root_path, *kind, &cand, &prepared) {
                        Ok(Decision::Reclaim) => {
                            report.record(&path, Decision::Reclaim, bytes);
                            Decision::Reclaim
                        }
                        Ok(other) => {
                            report.record_with(&path, other, 0, pass.failure.take());
                            other
                        }
                        Err(_) => {
                            report.record(&path, Decision::Failed, 0);
                            Decision::Failed
                        }
                    };
                    if build_target {
                        report.note_build_target_owned(
                            &cand.name,
                            outcome,
                            Some(bytes),
                            idle_ns,
                            pass.owner.take(),
                        );
                    }
                }
            }
        }
    }
    // Finish the registry removal of allocations a reclaim completed (#1171):
    // only tombstoned entries, under the custody lock reclaims also take.
    if config.registry_prune
        && !dry_run
        && let Some(registry) = &pass.registry
    {
        registry.prune(PRUNE_PER_PASS, &mut pass.budget);
    }
    report.budget_exhausted |= pass.budget.is_exhausted();
    report
}
