//! Warm, persistent cargo targets for merge-queue gates (#2467).
//!
//! Every gate used to build into a fresh, private pair of targets (`test` and
//! `check`) that the lander deleted with its workspace, so each gate compiled
//! every third-party crate and every workspace crate again: the prebuild and
//! the compile checks, which run side by side, were the gate's critical path at
//! 200-250 s. A gate now leases one *slot* under `<queue target>/warm/` and
//! builds in that slot's fixed `source` checkout into its two targets, which
//! survive between gates. Candidate construction and publication still use
//! the integration engine's private worktrees.
//!
//! **Concurrency.** A slot is leased through an exclusive `flock` on
//! `slot-N.lock` for the lease's whole life (the lander process). A lander that
//! finds no free slot builds cold into its private targets, as before, so two
//! gates never write one target.
//!
//! **Correctness.** Cargo records absolute paths for inputs outside a package
//! and build scripts can generate includes relative to their OUT_DIR. Every
//! build in a slot must use the same checkout path (#2490, #2491). Binding
//! replaces `source` with a clean, shared clone of the candidate's exact HEAD;
//! its object store is borrowed only for the current gate and replaced at the
//! next bind. This avoids coupling a persistent Git worktree registration to
//! the lander's temporary repository. Old-format slots are emptied once.
//! A new checkout makes every source file newer than the
//! slot's artifacts and the workspace crates would rebuild: only the third-party
//! crates would be reused. To reuse the workspace crates that did not change,
//! the slot records, after a build that succeeded, the git blob id and the mtime
//! of every tracked file and directory of the tree it built (`manifest.json`).
//! The next gate gives each file whose blob id is unchanged its recorded mtime
//! back and leaves every other file at its checkout time, which is exactly the
//! state of one persistent checkout whose changed files Git rewrote.
//! Rules that keep this sound:
//!
//! * The manifest is deleted when a gate binds and written back only when that
//!   gate's compile checks and prebuild succeeded, so a gate that failed or was
//!   killed leaves no manifest and the next gate rebuilds every workspace crate
//!   (third-party crates stay warm).
//! * The manifest records the mtimes the tree had when the build started, so a
//!   file changed by a gate is newer than the artifacts of every earlier gate.
//! * A blob id fixes a file's bytes only while nothing else decides them, and
//!   Cargo fingerprints the bytes it reads through the path, following links
//!   (#2508). The manifest therefore also records every tracked symlink with
//!   its link text, every tracked `.gitattributes` and a digest of the git
//!   settings that convert files at checkout (`core.autocrlf`, `core.eol`,
//!   `filter.*`, the attribute files outside the tree). When any of them differs
//!   from the candidate's, no mtime is given back and the whole workspace
//!   rebuilds, which is the cold result. That is rare and cheap next to a
//!   false pass.
//! * [`WarmSlot::bind`] refuses a checkout whose HEAD is not the candidate's,
//!   and the caller builds cold.
//! * The slot record also binds artifacts to the checkout's absolute path;
//!   relocating a slot empties its targets before restoring any mtimes.
//! * A toolchain change (`rustc -vV` or `rust-toolchain.toml`) empties the
//!   slot; a lockfile change does not, because Cargo already keys every
//!   third-party artifact on its exact resolved package.
//!
//! **Bounds.** A slot over [`MAX_SLOT_BYTES`] is emptied at its next lease, and
//! the daemon's reclaim pass ([`run_gc_and_log`]) removes slots idle for
//! [`IDLE_TTL`] (or [`PRESSURE_MIN_IDLE`] under disk pressure), skipping any
//! slot a lander holds.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::shared_target_prune::try_lock_exclusive;

/// Directory under the queue's cargo target that holds the slots.
pub const WARM_DIR: &str = "warm";
/// Slots a lander may lease when `RSI_LANDER_WARM_SLOTS` is unset.
pub const DEFAULT_SLOTS: usize = 2;
/// Hard ceiling on `RSI_LANDER_WARM_SLOTS`.
pub const MAX_SLOTS: usize = 8;
/// A slot (both targets) over this is emptied at its next lease. A typical
/// slot holds 7-8 GiB.
pub const MAX_SLOT_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// An unleased slot unused for this long is reclaimed.
pub const IDLE_TTL: Duration = Duration::from_secs(72 * 3600);
/// Under disk pressure an unleased slot unused for this long is reclaimed.
pub const PRESSURE_MIN_IDLE: Duration = Duration::from_secs(10 * 60);
/// Directory entries one size measurement may visit before the slot counts as
/// oversized.
const SCAN_ENTRY_BUDGET: u64 = 3_000_000;
const SCHEMA: u32 = 3;
const STATE_FILE: &str = "state.json";
const MANIFEST_FILE: &str = "manifest.json";
const TEST_TARGET: &str = "test-target";
const CHECK_TARGET: &str = "check-target";
/// The desktop lane's Tauri Cargo target (hub #3023): slot-owned like the other
/// two, with the candidate's own crates trimmed after every lane run.
const DESKTOP_TARGET: &str = "desktop-target";
const SOURCE: &str = "source";

/// Whether gates use warm targets at all. On by default: the delta soundness
/// review #2507 of #2490/#2491 left #2508 as the only blocker, and it is fixed.
/// `RSI_LANDER_WARM_TARGET` set to `0`, `false`, `off`, `no`, `disable` or
/// `disabled` (any case) opts out and keeps the cold path, identical to the
/// lander before #2467; unset or any other value leaves the warm path on.
#[must_use]
pub fn enabled() -> bool {
    enabled_from(std::env::var("RSI_LANDER_WARM_TARGET").ok().as_deref())
}

fn enabled_from(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        !["0", "false", "off", "no", "disable", "disabled"]
            .iter()
            .any(|off| value.trim().eq_ignore_ascii_case(off))
    })
}

/// Slots a lander may lease: `RSI_LANDER_WARM_SLOTS` (0 disables), else
/// [`DEFAULT_SLOTS`], never above [`MAX_SLOTS`].
#[must_use]
pub fn configured_slots() -> usize {
    std::env::var("RSI_LANDER_WARM_SLOTS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_SLOTS)
        .min(MAX_SLOTS)
}

/// A file's or directory's mtime, with the identity of what it was taken from
/// (a git blob id for a file, a hash of the children for a directory).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    id: String,
    secs: i64,
    nanos: u32,
}

impl Stamp {
    fn new(id: &str, time: SystemTime) -> Option<Self> {
        let since = time.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            id: id.to_owned(),
            secs: i64::try_from(since.as_secs()).ok()?,
            nanos: since.subsec_nanos(),
        })
    }

    fn time(&self) -> Option<SystemTime> {
        let secs = u64::try_from(self.secs).ok()?;
        UNIX_EPOCH.checked_add(Duration::new(secs, self.nanos))
    }
}

/// Everything besides a regular file's own blob id that decides the bytes
/// Cargo reads at a tracked path (#2508). Two trees with equal records have
/// equal bytes wherever their blob ids are equal; unequal records give no
/// mtime back.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Checkout {
    /// Every tracked symlink: path to the blob id of its link text. Cargo
    /// follows the link, so a retarget changes the bytes read at the path while
    /// the file it now names keeps its own blob id and mtime.
    symlinks: BTreeMap<String, String>,
    /// Every tracked `.gitattributes`: path to blob id. `eol`, `text`,
    /// `filter` and `working-tree-encoding` change checked-out bytes without
    /// changing any blob id.
    attributes: BTreeMap<String, String>,
    /// Digest of the entries `git ls-files` listed that no key could be made
    /// for (a path that is not UTF-8); empty when there were none.
    unkeyed: String,
    /// Digest of the git settings outside the tree that convert files at
    /// checkout ([`checkout_settings`]).
    settings: String,
}

/// What the last successful build left in a slot: the mtime of every tracked
/// file and directory of the tree it built.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    checkout: Checkout,
    files: BTreeMap<String, Stamp>,
    dirs: BTreeMap<String, Stamp>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SlotRecord {
    schema: u32,
    toolchain: String,
    checkout: PathBuf,
}

/// What a slot looked like when a gate leased it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// No usable record: the slot was empty or half-deleted.
    Cold,
    /// The previous toolchain's artifacts were removed.
    ToolchainChanged,
    /// The slot exceeded [`MAX_SLOT_BYTES`] and was emptied.
    OverSize,
    /// The slot's artifacts were kept.
    Warm,
}

impl SlotState {
    fn label(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::ToolchainChanged => "toolchain_changed",
            Self::OverSize => "over_size",
            Self::Warm => "warm",
        }
    }
}

/// The result of binding a slot to a candidate worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindOutcome {
    /// Fixed checkout path all commands of this gate must use.
    pub worktree: PathBuf,
    pub state: SlotState,
    /// Bytes in the slot's targets after any reset.
    pub bytes: u64,
    /// Tracked files whose recorded mtime was given back.
    pub restored_files: usize,
    /// Tracked files in the candidate tree.
    pub files: usize,
    pub restored_dirs: usize,
    pub dirs: usize,
    /// Whether the previous gate left a manifest.
    pub manifest_found: bool,
    /// The manifest named a different set of symlinks, `.gitattributes` or
    /// checkout settings than this candidate has (#2508), so no mtime was
    /// given back.
    pub checkout_changed: bool,
}

impl BindOutcome {
    /// The one `warm_target` line the lander prints per bound gate.
    #[must_use]
    pub fn evidence(&self, slot: usize) -> String {
        format!(
            "warm_target slot={slot} state={} bytes={} manifest={} restored_files={}/{} restored_dirs={}/{} checkout_changed={}",
            self.state.label(),
            self.bytes,
            self.manifest_found,
            self.restored_files,
            self.files,
            self.restored_dirs,
            self.dirs,
            self.checkout_changed,
        )
    }
}

/// A leased slot. Dropping it releases the lease.
pub struct WarmSlot {
    index: usize,
    dir: PathBuf,
    max_bytes: u64,
    /// The tree stamped by the last [`WarmSlot::bind`], written by
    /// [`WarmSlot::commit`].
    pending: Option<Manifest>,
    /// What the last [`WarmSlot::bind`] found, for the desktop target's evidence.
    bound: Option<SlotState>,
    /// The slot directory opened once at acquisition with
    /// `O_DIRECTORY | O_NOFOLLOW`. Trim and reset act through this descriptor,
    /// never through `dir`'s pathname, so a slot directory renamed away and
    /// replaced by a symlink after the lease cannot redirect them (#3187).
    anchor: File,
    _lease: File,
}

impl WarmSlot {
    /// Lease the lowest free slot under `warm_root` (created when absent).
    /// `None` when `slots` is 0, every slot is leased or the directory cannot
    /// be used: the caller then builds cold into private targets.
    #[must_use]
    pub fn acquire(warm_root: &Path, slots: usize) -> Option<Self> {
        let slots = slots.min(MAX_SLOTS);
        if slots == 0 || ensure_private_dir(warm_root).is_err() {
            return None;
        }
        for index in 0..slots {
            let Ok(lease) = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(lock_path(warm_root, index))
            else {
                continue;
            };
            if !matches!(try_lock_exclusive(&lease), Ok(true)) {
                continue;
            }
            let dir = slot_dir(warm_root, index);
            if ensure_private_dir(&dir).is_err() {
                continue;
            }
            let Ok(dir) = fs::canonicalize(&dir) else {
                continue;
            };
            let Ok(anchor) = open_anchor(&dir) else {
                continue;
            };
            return Some(Self {
                index,
                dir,
                max_bytes: MAX_SLOT_BYTES,
                pending: None,
                bound: None,
                anchor,
                _lease: lease,
            });
        }
        None
    }

    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// The target `cargo test`/`nextest` builds use.
    #[must_use]
    pub fn test_target(&self) -> PathBuf {
        self.dir.join(TEST_TARGET)
    }

    /// The target `cargo check` builds use.
    #[must_use]
    pub fn check_target(&self) -> PathBuf {
        self.dir.join(CHECK_TARGET)
    }

    /// The target the desktop lane's `cargo test` in `desktop/src-tauri` uses
    /// (hub #3023).
    #[must_use]
    pub fn desktop_target(&self) -> PathBuf {
        self.dir.join(DESKTOP_TARGET)
    }

    /// The one `desktop_warm_target` line the lander prints before the lane
    /// runs: the slot and whether the desktop target is warm (kept from an
    /// earlier gate) or empty, with its size.
    #[must_use]
    pub fn desktop_evidence(&self) -> String {
        let target = self.desktop_target();
        let populated = fs::read_dir(&target).is_ok_and(|mut entries| entries.next().is_some());
        let mut budget = SCAN_ENTRY_BUDGET;
        let (bytes, _) = tree_bytes(&target, &mut budget);
        format!(
            "desktop_warm_target slot={} state={} bytes={bytes}",
            self.index,
            desktop_state(self.bound, populated),
        )
    }

    /// Remove what the lane built of the candidate's own crates (`crates` are
    /// the names its `Cargo.toml` declares), keeping every third-party
    /// artifact, which Cargo keys on the locked package. With no names the
    /// whole desktop target goes, so an unreadable manifest can only make the
    /// next gate cold. Returns the entries removed.
    ///
    /// # Errors
    /// An artifact cannot be removed; the target is then removed whole. A
    /// symlink or non-directory where the layout needs a directory also removes
    /// the target whole (never following the link) and is not an error.
    pub fn trim_desktop(&self, crates: &[String]) -> io::Result<usize> {
        let anchor = self.anchor.as_raw_fd();
        // A name that is not a plain crate identifier (empty, a path, a dot
        // name) could match entries it does not own, so it is treated like an
        // unreadable manifest: the whole target goes, nothing else.
        if crates.is_empty() || !crates.iter().all(|name| is_plain_crate_name(name)) {
            remove_entry_at(anchor, DESKTOP_TARGET, 0)?;
            return Ok(0);
        }
        let result = trim_candidate_artifacts(anchor, DESKTOP_TARGET, crates);
        if let Err(error) = &result {
            remove_entry_at(anchor, DESKTOP_TARGET, 0)?;
            // A symlink or non-directory in the layout is not a failure of the
            // gate: the target was emptied (a link unlinked, never followed),
            // so the next lane starts cold.
            if error.kind() == io::ErrorKind::InvalidData {
                return Ok(0);
            }
        }
        result
    }

    /// Materialize the candidate's HEAD at the slot's fixed checkout path,
    /// empty obsolete or oversized targets, then restore unchanged mtimes.
    /// All gate commands must use the returned `worktree`, before building.
    ///
    /// # Errors
    /// The toolchain cannot be probed, the slot cannot be emptied or the
    /// worktree's tracked files cannot be listed. The caller builds cold.
    pub fn bind(&mut self, worktree: &Path) -> io::Result<BindOutcome> {
        let toolchain = toolchain_key(worktree)?;
        self.bind_with_toolchain(worktree, &toolchain)
    }

    /// [`WarmSlot::bind`] with the toolchain identity supplied.
    ///
    /// # Errors
    /// See [`WarmSlot::bind`].
    pub fn bind_with_toolchain(
        &mut self,
        worktree: &Path,
        toolchain: &str,
    ) -> io::Result<BindOutcome> {
        // A failed bind must leave nothing to commit.
        self.pending = None;
        // Everything below that is path-based (the record, the manifest, the
        // checkout) must still reach the leased directory (#3187).
        self.ensure_anchored()?;
        let checkout = self.dir.join(SOURCE);
        let record: Option<SlotRecord> = read_json(&self.dir.join(STATE_FILE));
        let mut state = match &record {
            None => SlotState::Cold,
            Some(record) if record.schema != SCHEMA || record.checkout != checkout => {
                SlotState::Cold
            }
            Some(record) if record.toolchain != toolchain => SlotState::ToolchainChanged,
            Some(_) => SlotState::Warm,
        };
        if state != SlotState::Warm {
            self.reset()?;
        }
        let (mut bytes, exhausted) = self.measure();
        if state == SlotState::Warm && (exhausted || bytes > self.max_bytes) {
            self.reset()?;
            state = SlotState::OverSize;
            bytes = 0;
        }
        self.bound = Some(state);
        // The manifest describes the state the last successful build left.
        // Take it off disk first: if this gate dies mid-build the next gate
        // must find none.
        let manifest: Option<Manifest> = read_json(&self.dir.join(MANIFEST_FILE))
            .filter(|manifest: &Manifest| manifest.schema == SCHEMA);
        remove_entry_at(self.anchor.as_raw_fd(), MANIFEST_FILE, 0)?;
        write_json(
            &self.dir.join(STATE_FILE),
            &SlotRecord {
                schema: SCHEMA,
                toolchain: toolchain.to_owned(),
                checkout: checkout.clone(),
            },
        )?;
        self.checkout_candidate(worktree, &checkout)?;
        let manifest_found = manifest.is_some() && state == SlotState::Warm;
        let previous = manifest.filter(|_| state == SlotState::Warm);
        let (stamped, counts) = stamp_worktree(&checkout, previous.as_ref())?;
        self.pending = Some(stamped);
        Ok(BindOutcome {
            worktree: checkout,
            state,
            bytes,
            restored_files: counts.restored_files,
            files: counts.files,
            restored_dirs: counts.restored_dirs,
            dirs: counts.dirs,
            manifest_found,
            checkout_changed: counts.checkout_changed,
        })
    }

    /// Each gate borrows its private repository's objects, without copying
    /// history or leaving persistent linked-worktree registrations behind.
    /// Only the pathname persists: remove untracked build/test output too.
    fn checkout_candidate(&self, candidate: &Path, checkout: &Path) -> io::Result<()> {
        let candidate = fs::canonicalize(candidate)?;
        if candidate.starts_with(checkout) {
            return Err(io::Error::other("candidate is inside the warm checkout"));
        }
        let expected = head_commit(&candidate)?;
        if checkout.exists() {
            ensure_private_dir(checkout)?;
            remove_entry_at(self.anchor.as_raw_fd(), SOURCE, 0)?;
        }
        let output = git_command(None)
            .args(["clone", "--shared", "--quiet", "--"])
            .arg(&candidate)
            .arg(checkout)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "warm checkout clone failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        ensure_head(checkout, &expected)
    }

    /// Record the bound worktree's mtimes after a build that succeeded, so the
    /// next gate can reuse the workspace crates that did not change. Returns
    /// the number of tracked files recorded (0 when nothing was bound).
    ///
    /// # Errors
    /// The manifest cannot be written; the next gate then rebuilds the
    /// workspace crates.
    pub fn commit(&mut self) -> io::Result<usize> {
        let Some(manifest) = self.pending.take() else {
            return Ok(0);
        };
        write_json(&self.dir.join(MANIFEST_FILE), &manifest)?;
        Ok(manifest.files.len())
    }

    /// Remove everything a previous gate left (state, manifest, both targets).
    /// The record goes first, so a deletion that stops partway reads as cold.
    /// Every removal goes through the lease-time directory descriptor.
    fn reset(&self) -> io::Result<()> {
        let anchor = self.anchor.as_raw_fd();
        for name in [
            STATE_FILE,
            MANIFEST_FILE,
            TEST_TARGET,
            CHECK_TARGET,
            DESKTOP_TARGET,
        ] {
            remove_entry_at(anchor, name, 0)?;
        }
        Ok(())
    }

    /// Whether the slot's pathname still names the directory leased at
    /// acquisition. A slot renamed away and replaced (by a symlink or another
    /// directory) is refused rather than built into.
    fn ensure_anchored(&self) -> io::Result<()> {
        let held = self.anchor.metadata()?;
        let named = fs::symlink_metadata(&self.dir)?;
        if named.file_type().is_symlink()
            || !named.is_dir()
            || (named.dev(), named.ino()) != (held.dev(), held.ino())
        {
            return Err(io::Error::other(format!(
                "slot directory {} is no longer the leased directory",
                self.dir.display()
            )));
        }
        Ok(())
    }

    /// Bytes in both targets and whether the entry budget ran out.
    fn measure(&self) -> (u64, bool) {
        let mut budget = SCAN_ENTRY_BUDGET;
        let mut bytes = 0_u64;
        for target in [
            self.test_target(),
            self.check_target(),
            self.desktop_target(),
        ] {
            let (found, exhausted) = tree_bytes(&target, &mut budget);
            bytes = bytes.saturating_add(found);
            if exhausted {
                return (bytes, true);
            }
        }
        (bytes, false)
    }
}

/// `warm` only when the slot was kept at bind and the desktop target still
/// holds artifacts; otherwise the reason the slot was emptied, or `cold`.
fn desktop_state(bound: Option<SlotState>, populated: bool) -> &'static str {
    match bound {
        Some(SlotState::Warm) if populated => SlotState::Warm.label(),
        Some(SlotState::Warm) | None => SlotState::Cold.label(),
        Some(state) => state.label(),
    }
}

/// The crate names a `Cargo.toml` declares for itself: the package, its library
/// and its binaries, which are what a candidate changes (a registry crate is
/// fixed by `Cargo.lock`). Empty when the manifest does not parse.
#[must_use]
pub fn declared_crate_names(cargo_toml: &str) -> Vec<String> {
    let Ok(manifest) = cargo_toml.parse::<toml::Table>() else {
        return Vec::new();
    };
    let name = |table: Option<&toml::Value>| {
        table
            .and_then(|table| table.get("name"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    let mut names: Vec<String> = [name(manifest.get("package")), name(manifest.get("lib"))]
        .into_iter()
        .flatten()
        .collect();
    if let Some(bins) = manifest.get("bin").and_then(toml::Value::as_array) {
        names.extend(bins.iter().filter_map(|bin| name(Some(bin))));
    }
    names.sort();
    names.dedup();
    names
}

/// The most manifests one walk reads; a larger graph is not a desktop lane.
const MAX_WALKED_MANIFESTS: usize = 256;

/// Every dependency table of a parsed manifest (`dependencies`,
/// `dev-dependencies`, `build-dependencies`, the same under `target.*`, and
/// `workspace.dependencies`), as `scripts/desktop-test-gate.py` reads them.
fn dependency_tables(manifest: &toml::Table) -> Vec<&toml::Table> {
    const NAMES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut tables: Vec<Option<&toml::Value>> = NAMES.iter().map(|n| manifest.get(*n)).collect();
    tables.push(
        manifest
            .get("workspace")
            .and_then(toml::Value::as_table)
            .and_then(|workspace| workspace.get("dependencies")),
    );
    let targets = manifest.get("target").and_then(toml::Value::as_table);
    for target in targets.into_iter().flat_map(toml::Table::values) {
        let Some(target) = target.as_table() else {
            continue;
        };
        tables.extend(NAMES.iter().map(|n| target.get(*n)));
    }
    tables
        .into_iter()
        .filter_map(|table| table.and_then(toml::Value::as_table))
        .collect()
}

/// `base` joined with `relative`, lexically normalised, as a path under the
/// checkout root; `None` when it leaves the root or names the root itself.
fn normalized_under_root(base: &Path, relative: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut parts: Vec<&std::ffi::OsStr> = base
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.into_iter().collect())
}

/// The checkout-relative directories a manifest reaches through `path = ...`
/// dependencies. A `workspace = true` dependency takes its path from
/// `workspace_paths` (the root manifest's `workspace.dependencies`, relative
/// to the root). A path leaving the checkout is dropped.
fn path_dependency_dirs(
    manifest: &toml::Table,
    manifest_dir: &Path,
    workspace_paths: &BTreeMap<String, PathBuf>,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for table in dependency_tables(manifest) {
        for (name, spec) in table {
            let Some(spec) = spec.as_table() else {
                continue;
            };
            if let Some(path) = spec.get("path").and_then(toml::Value::as_str) {
                found.extend(normalized_under_root(manifest_dir, path));
                continue;
            }
            if spec.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                found.extend(workspace_paths.get(name).cloned());
            }
        }
    }
    found
}

/// The crate names of a manifest and of every `path = ...` dependency it
/// reaches, transitively, inside `root`: what a desktop candidate may have
/// changed (a registry crate is fixed by `Cargo.lock`). `manifest` is relative
/// to `root`. Empty when the manifest, or any path dependency manifest it
/// names, cannot be read or parsed, so the caller trims the whole target.
#[must_use]
pub fn candidate_crate_names(root: &Path, manifest: &Path) -> Vec<String> {
    let read = |directory: &Path| -> Option<(String, toml::Table)> {
        let text = fs::read_to_string(root.join(directory).join("Cargo.toml")).ok()?;
        let table = text.parse::<toml::Table>().ok()?;
        Some((text, table))
    };
    let workspace_paths: BTreeMap<String, PathBuf> = read(Path::new(""))
        .map(|(_, root_manifest)| {
            root_manifest
                .get("workspace")
                .and_then(toml::Value::as_table)
                .and_then(|workspace| workspace.get("dependencies"))
                .and_then(toml::Value::as_table)
                .into_iter()
                .flatten()
                .filter_map(|(name, spec)| {
                    let path = spec.as_table()?.get("path")?.as_str()?;
                    Some((name.clone(), normalized_under_root(Path::new(""), path)?))
                })
                .collect()
        })
        .unwrap_or_default();
    let start = manifest.parent().unwrap_or(Path::new("")).to_path_buf();
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = vec![start];
    while let Some(directory) = queue.pop() {
        if !seen.insert(directory.clone()) {
            continue;
        }
        if seen.len() > MAX_WALKED_MANIFESTS {
            return Vec::new();
        }
        let Some((text, table)) = read(&directory) else {
            return Vec::new();
        };
        names.extend(declared_crate_names(&text));
        queue.extend(path_dependency_dirs(&table, &directory, &workspace_paths));
    }
    names.sort();
    names.dedup();
    names
}

/// Whether an entry of a Cargo target directory is an artifact of one of
/// `crates`: `name-<hex>[.ext]` (also `lib<name>-<hex>`) under `deps`,
/// `.fingerprint`, `build` and `incremental`, or `name[.ext]` /
/// `lib<name>[.ext]` at a profile's top level. A hyphen and an underscore name
/// the same crate. The hash must be hex, so `rsi-desktop-extra` is not
/// `rsi-desktop`.
fn is_candidate_artifact(entry: &str, crates: &[String]) -> bool {
    let bare = entry.strip_prefix("lib").unwrap_or(entry);
    crates.iter().any(|name| {
        [name.as_str(), &name.replace('-', "_")]
            .into_iter()
            .any(|spelling| {
                [entry, bare].into_iter().any(|stem| {
                    let Some(rest) = stem.strip_prefix(spelling) else {
                        return false;
                    };
                    if rest.is_empty() || rest.starts_with('.') {
                        return true;
                    }
                    let Some(hashed) = rest.strip_prefix('-') else {
                        return false;
                    };
                    let hash = hashed.split('.').next().unwrap_or_default();
                    hash.len() >= 8 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
    })
}

/// Whether `name` is shaped like a Cargo package, library or binary name:
/// non-empty ASCII letters, digits, `-` and `_`.
fn is_plain_crate_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Why a desktop target cannot be trimmed in place: a symlink or non-directory
/// sits where the layout needs a real directory, or the tree is implausibly
/// deep. The caller then removes the target whole, which unlinks a symlink
/// without ever following it.
fn invalid_layout(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("desktop target layout is not a plain directory tree: {what}"),
    )
}

/// Open the slot directory `dir` as the lease's anchor descriptor.
fn open_anchor(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(dir)
}

/// Deepest directory nesting the contained removal follows.
const MAX_REMOVE_DEPTH: usize = 128;

/// Open `name` under `parent` (the current directory when `None`) as a
/// directory without following a symlink. `Ok(None)` when it is absent; an
/// error when a symlink or any other non-directory is there.
fn open_dir_nofollow(parent: Option<RawFd>, name: &str) -> io::Result<Option<nix::dir::Dir>> {
    use nix::errno::Errno;
    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    match nix::dir::Dir::openat(parent, name, flags, Mode::empty()) {
        Ok(dir) => Ok(Some(dir)),
        Err(Errno::ENOENT) => Ok(None),
        Err(Errno::ELOOP | Errno::ENOTDIR) => Err(invalid_layout(name)),
        Err(errno) => Err(io::Error::from(errno)),
    }
}

/// Names of the entries of `dir` that are not `.` or `..`. Names that are not
/// UTF-8 cannot be a crate's artifact and are left out.
fn dir_entry_names(dir: &mut nix::dir::Dir) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in dir.iter() {
        let entry = entry.map_err(io::Error::from)?;
        let raw = entry.file_name().to_bytes();
        if raw == b"." || raw == b".." {
            continue;
        }
        if let Ok(name) = std::str::from_utf8(raw) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Whether `name` under `parent` is a real directory (a symlink is not).
fn entry_is_real_dir(parent: RawFd, name: &str) -> io::Result<bool> {
    use nix::fcntl::AtFlags;
    use nix::sys::stat::{SFlag, fstatat};
    match fstatat(Some(parent), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR),
        Err(nix::errno::Errno::ENOENT) => Ok(false),
        Err(errno) => Err(io::Error::from(errno)),
    }
}

/// Remove `name` under `parent` without following any symlink: a symlink is
/// unlinked itself, a directory is emptied through descriptors opened with
/// `O_NOFOLLOW` at every level, so a component swapped for a symlink mid-walk
/// is refused instead of followed.
fn remove_entry_at(parent: RawFd, name: &str, depth: usize) -> io::Result<()> {
    use nix::unistd::{UnlinkatFlags, unlinkat};
    if !entry_is_real_dir(parent, name)? {
        return match unlinkat(Some(parent), name, UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(nix::errno::Errno::ENOENT) => Ok(()),
            Err(errno) => Err(io::Error::from(errno)),
        };
    }
    if depth >= MAX_REMOVE_DEPTH {
        return Err(invalid_layout("too deep"));
    }
    let Some(mut dir) = open_dir_nofollow(Some(parent), name)? else {
        return Ok(());
    };
    for child in dir_entry_names(&mut dir)? {
        remove_entry_at(dir.as_raw_fd(), &child, depth + 1)?;
    }
    drop(dir);
    match unlinkat(Some(parent), name, UnlinkatFlags::RemoveDir) {
        Ok(()) | Err(nix::errno::Errno::ENOENT) => Ok(()),
        Err(errno) => Err(io::Error::from(errno)),
    }
}

/// Remove the candidate crates' artifacts from every profile directory of
/// `target`, a name under the leased slot's descriptor `slot`; returns the
/// entries removed. A target that does not exist trims nothing. Every directory
/// is opened relative to its parent's descriptor without following a symlink,
/// starting at the slot descriptor, so nothing outside the leased directory is
/// read or removed; a symlink where a directory belongs is an error (the
/// caller then removes the target whole).
fn trim_candidate_artifacts(slot: RawFd, target: &str, crates: &[String]) -> io::Result<usize> {
    let Some(mut root) = open_dir_nofollow(Some(slot), target)? else {
        return Ok(0);
    };
    let root_fd = root.as_raw_fd();
    let mut removed = 0;
    for profile in dir_entry_names(&mut root)? {
        // Files at the root (`CACHEDIR.TAG`, `.rustc_info.json`) are not
        // profiles; a symlink is never followed.
        let Some(mut profile_dir) = open_profile_dir(root_fd, &profile)? else {
            continue;
        };
        let profile_fd = profile_dir.as_raw_fd();
        removed += remove_matching(&mut profile_dir, crates)?;
        for sub in ["deps", ".fingerprint", "build", "incremental"] {
            if let Some(mut sub_dir) = open_dir_nofollow(Some(profile_fd), sub)? {
                removed += remove_matching(&mut sub_dir, crates)?;
            }
        }
    }
    Ok(removed)
}

/// A profile directory of the target: `None` for an entry that is a plain file;
/// an error for a symlink.
fn open_profile_dir(root: RawFd, name: &str) -> io::Result<Option<nix::dir::Dir>> {
    use nix::fcntl::AtFlags;
    use nix::sys::stat::{SFlag, fstatat};
    let stat = fstatat(Some(root), name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    match SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT {
        SFlag::S_IFDIR => open_dir_nofollow(Some(root), name),
        SFlag::S_IFLNK => Err(invalid_layout(name)),
        _ => Ok(None),
    }
}

fn remove_matching(directory: &mut nix::dir::Dir, crates: &[String]) -> io::Result<usize> {
    let fd = directory.as_raw_fd();
    let mut removed = 0;
    for name in dir_entry_names(directory)? {
        if !is_candidate_artifact(&name, crates) {
            continue;
        }
        remove_entry_at(fd, &name, 0)?;
        removed += 1;
    }
    Ok(removed)
}

fn lock_path(warm_root: &Path, index: usize) -> PathBuf {
    warm_root.join(format!("slot-{index}.lock"))
}

fn slot_dir(warm_root: &Path, index: usize) -> PathBuf {
    warm_root.join(format!("slot-{index}"))
}

/// A `git` command with the repository-selecting environment removed, run in
/// `cwd` when given.
fn git_command(cwd: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null());
    command
}

/// The commit `repo`'s HEAD names.
fn head_commit(repo: &Path) -> io::Result<String> {
    let output = git_command(Some(repo))
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .output()?;
    let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || head.is_empty() {
        return Err(io::Error::other(format!(
            "{} has no HEAD commit",
            repo.display()
        )));
    }
    Ok(head)
}

/// The slot's checkout must hold exactly the candidate's commit (#2508): a
/// clone that landed anywhere else would build, and pass or fail, a tree the
/// gate is not testing.
fn ensure_head(checkout: &Path, expected: &str) -> io::Result<()> {
    let actual = head_commit(checkout)?;
    if actual == expected {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "warm checkout is at {actual}, not the candidate's {expected}"
    )))
}

/// Create `path` (mode 0700) when absent; refuse a symlink, a non-directory or
/// a directory another user owns.
fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(io::Error::other(format!(
                    "{} is a symlink or not a directory",
                    path.display()
                )));
            }
            if meta.uid() != nix::unistd::geteuid().as_raw() {
                return Err(io::Error::other(format!(
                    "{} is not owned by this user",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path)
        }
        Err(error) => Err(error),
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Write through a temporary file and a rename, so a reader never sees half a
/// document.
fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(value).map_err(io::Error::other)?)?;
    file.flush()?;
    drop(file);
    fs::rename(&temporary, path)
}

/// Total file bytes under `path` (links not followed). The flag is true when
/// `budget` entries were visited before the walk ended.
fn tree_bytes(path: &Path, budget: &mut u64) -> (u64, bool) {
    let mut bytes = 0_u64;
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
    {
        if *budget == 0 {
            return (bytes, true);
        }
        *budget -= 1;
        if let Ok(entry) = entry
            && entry.file_type().is_file()
            && let Ok(meta) = entry.metadata()
        {
            bytes = bytes.saturating_add(meta.len());
        }
    }
    (bytes, false)
}

/// Identity of the toolchain a gate builds with: the pinned toolchain file and
/// the compiler's own `-vV` report, resolved the way the gate's cargo resolves
/// it (from the worktree).
fn toolchain_key(worktree: &Path) -> io::Result<String> {
    let report = probe_rustc(worktree)?;
    let mut hasher = Sha256::new();
    hasher.update(b"rsid-lander-warm-toolchain-v1\0");
    for name in ["rust-toolchain.toml", "rust-toolchain"] {
        if let Ok(contents) = fs::read(worktree.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            hasher.update(&contents);
            break;
        }
    }
    hasher.update(b"\0");
    hasher.update(report.as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

/// The compiler's `-vV` report, resolved from the worktree. A wedged toolchain
/// installer is bounded by the queue's gate wall-time budget, which stops the
/// lander group; the gate's own cargo would wait on the same installer.
fn probe_rustc(worktree: &Path) -> io::Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() || text.trim().is_empty() {
        return Err(io::Error::other("rustc -vV failed"));
    }
    Ok(text)
}

/// One entry of `git ls-files -s`.
struct Tracked {
    path: String,
    mode: u32,
    oid: String,
}

impl Tracked {
    fn regular(&self) -> bool {
        self.mode == 0o100_644 || self.mode == 0o100_755
    }

    fn symlink(&self) -> bool {
        self.mode == 0o120_000
    }

    fn attributes(&self) -> bool {
        Path::new(&self.path).file_name() == Some(std::ffi::OsStr::new(".gitattributes"))
    }
}

/// The tracked entries of `worktree` and a digest of the records that could
/// not be keyed (empty when every record could).
fn tracked_entries(worktree: &Path) -> io::Result<(Vec<Tracked>, String)> {
    let output = git_command(Some(worktree))
        .args(["ls-files", "-s", "-z"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("git ls-files failed"));
    }
    let mut entries = Vec::new();
    let mut unkeyed = Sha256::new();
    let mut any_unkeyed = false;
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(entry) = std::str::from_utf8(record).ok().and_then(parse_tracked) else {
            // A path that is not UTF-8 cannot be keyed; it is never restored,
            // but a change to one still has to change the manifest's identity.
            unkeyed.update(record);
            unkeyed.update([0]);
            any_unkeyed = true;
            continue;
        };
        if let Some(entry) = entry {
            entries.push(entry);
        }
    }
    let unkeyed = if any_unkeyed {
        hex::encode(unkeyed.finalize())
    } else {
        String::new()
    };
    Ok((entries, unkeyed))
}

/// One `git ls-files -s` record: `None` for a merge-conflict stage (never part
/// of a clone), the outer `None` for a record that cannot be parsed.
fn parse_tracked(record: &str) -> Option<Option<Tracked>> {
    let (meta, path) = record.split_once('\t')?;
    let mut fields = meta.split(' ');
    let (mode, oid, stage) = (fields.next()?, fields.next()?, fields.next()?);
    let mode = u32::from_str_radix(mode, 8).ok()?;
    if stage != "0" {
        return Some(None);
    }
    Some(Some(Tracked {
        path: path.to_owned(),
        mode,
        oid: oid.to_owned(),
    }))
}

/// Config keys whose values change the bytes a checkout writes.
const CHECKOUT_CONFIG_KEYS: &str = r"^(core\.(autocrlf|eol|attributesfile|symlinks|checkroundtripencoding|sparsecheckout)|filter\..+)$";

/// Digest of the git settings, outside the tree, that decide how a checkout
/// writes blobs to disk (#2508): the conversion config keys and the attribute
/// files git reads besides the tracked `.gitattributes`. Resolved the way the
/// clone resolves them, from `worktree`.
fn checkout_settings(worktree: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"rsid-lander-warm-checkout-settings-v1\0");
    // Exit status 1 only means that no key matched.
    let config = git_command(Some(worktree))
        .args(["config", "--null", "--get-regexp", CHECKOUT_CONFIG_KEYS])
        .stderr(Stdio::null())
        .output()?;
    if !matches!(config.status.code(), Some(0 | 1)) {
        return Err(io::Error::other("git config failed"));
    }
    hasher.update(&config.stdout);
    let query = |args: &[&str]| -> Option<PathBuf> {
        let output = git_command(Some(worktree))
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let text = text.trim();
        (output.status.success() && !text.is_empty()).then(|| worktree.join(text))
    };
    let mut sources: Vec<PathBuf> = [
        &["rev-parse", "--git-path", "info/attributes"][..],
        &["var", "GIT_ATTR_GLOBAL"][..],
        &["var", "GIT_ATTR_SYSTEM"][..],
        &["config", "--path", "--get", "core.attributesfile"][..],
    ]
    .iter()
    .filter_map(|args| query(*args))
    .collect();
    // Older gits cannot report the default global attributes file.
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
    {
        sources.push(config_home.join("git/attributes"));
    }
    sources.sort();
    sources.dedup();
    for source in sources {
        hasher.update(source.as_os_str().as_encoded_bytes());
        hasher.update(b"\0");
        match fs::read(&source) {
            Ok(contents) => hasher.update(&contents),
            Err(_) => hasher.update(b"absent"),
        }
        hasher.update(b"\0");
    }
    Ok(hex::encode(hasher.finalize()))
}

impl Checkout {
    fn of(worktree: &Path, entries: &[Tracked], unkeyed: String) -> io::Result<Self> {
        let mut checkout = Self {
            unkeyed,
            settings: checkout_settings(worktree)?,
            ..Self::default()
        };
        for entry in entries {
            if entry.symlink() {
                checkout
                    .symlinks
                    .insert(entry.path.clone(), entry.oid.clone());
            }
            if entry.attributes() {
                checkout
                    .attributes
                    .insert(entry.path.clone(), entry.oid.clone());
            }
        }
        Ok(checkout)
    }
}

/// Children of every directory of the tracked tree (`""` is the root): name,
/// a kind letter and, for a non-directory, its blob id.
fn directory_children(entries: &[Tracked]) -> BTreeMap<String, BTreeSet<(String, char, String)>> {
    let mut dirs: BTreeMap<String, BTreeSet<(String, char, String)>> = BTreeMap::new();
    dirs.entry(String::new()).or_default();
    for entry in entries {
        let parts: Vec<&str> = entry.path.split('/').collect();
        let Some((name, parents)) = parts.split_last() else {
            continue;
        };
        let mut parent = String::new();
        for part in parents {
            let child = if parent.is_empty() {
                (*part).to_owned()
            } else {
                format!("{parent}/{part}")
            };
            dirs.entry(parent.clone()).or_default().insert((
                (*part).to_owned(),
                'd',
                String::new(),
            ));
            dirs.entry(child.clone()).or_default();
            parent = child;
        }
        let kind = match entry.mode {
            0o120_000 => 'l',
            0o160_000 => 's',
            _ => 'f',
        };
        dirs.entry(parent)
            .or_default()
            .insert(((*name).to_owned(), kind, entry.oid.clone()));
    }
    dirs
}

fn directory_id(children: &BTreeSet<(String, char, String)>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rsid-lander-warm-dir-v1\0");
    for (name, kind, oid) in children {
        hasher.update(name.as_bytes());
        hasher.update([0, *kind as u8, 0]);
        hasher.update(oid.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

fn restore_mtime(path: &Path, time: SystemTime) -> io::Result<()> {
    File::open(path)?.set_times(FileTimes::new().set_modified(time))
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).ok()?.modified().ok()
}

#[derive(Default)]
struct StampCounts {
    files: usize,
    restored_files: usize,
    dirs: usize,
    restored_dirs: usize,
    checkout_changed: bool,
}

/// Give every tracked file and directory of `worktree` whose identity matches
/// `previous` its recorded mtime, and return the mtimes the tree now has.
/// Nothing is given back when `previous` was built from a different set of
/// symlinks, `.gitattributes` or checkout settings (#2508): a blob id then no
/// longer fixes the bytes Cargo reads at its path.
fn stamp_worktree(
    worktree: &Path,
    previous: Option<&Manifest>,
) -> io::Result<(Manifest, StampCounts)> {
    let (entries, unkeyed) = tracked_entries(worktree)?;
    let checkout = Checkout::of(worktree, &entries, unkeyed)?;
    let now = SystemTime::now();
    let mut counts = StampCounts {
        checkout_changed: previous.is_some_and(|manifest| manifest.checkout != checkout),
        ..StampCounts::default()
    };
    let previous = previous.filter(|manifest| manifest.checkout == checkout);
    let mut manifest = Manifest {
        schema: SCHEMA,
        checkout,
        ..Manifest::default()
    };
    for entry in entries.iter().filter(|entry| entry.regular()) {
        let path = worktree.join(&entry.path);
        counts.files += 1;
        let recorded = previous
            .and_then(|manifest| manifest.files.get(&entry.path))
            .filter(|stamp| stamp.id == entry.oid)
            .and_then(Stamp::time)
            .filter(|time| *time <= now);
        let restored = recorded.filter(|time| restore_mtime(&path, *time).is_ok());
        let time = match restored {
            Some(time) => {
                counts.restored_files += 1;
                time
            }
            None => match modified(&path) {
                Some(time) => time,
                None => continue,
            },
        };
        if let Some(stamp) = Stamp::new(&entry.oid, time) {
            manifest.files.insert(entry.path.clone(), stamp);
        }
    }
    for (directory, children) in directory_children(&entries) {
        let path = if directory.is_empty() {
            worktree.to_path_buf()
        } else {
            worktree.join(&directory)
        };
        counts.dirs += 1;
        let id = directory_id(&children);
        let recorded = previous
            .and_then(|manifest| manifest.dirs.get(&directory))
            .filter(|stamp| stamp.id == id)
            .and_then(Stamp::time)
            .filter(|time| *time <= now);
        let restored = recorded.filter(|time| restore_mtime(&path, *time).is_ok());
        let time = match restored {
            Some(time) => {
                counts.restored_dirs += 1;
                time
            }
            None => match modified(&path) {
                Some(time) => time,
                None => continue,
            },
        };
        if let Some(stamp) = Stamp::new(&id, time) {
            manifest.dirs.insert(directory, stamp);
        }
    }
    Ok((manifest, counts))
}

/// Slots a reclaim pass looked at.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub removed: Vec<usize>,
    pub kept: Vec<usize>,
    /// Leased by a running lander; never touched.
    pub in_use: Vec<usize>,
}

/// Remove the unleased slots under `warm_root` that have been idle long enough
/// (`pressure` shortens the wait). A slot is removed only while this pass holds
/// its lock, so a lander cannot lease it half-deleted.
#[must_use]
pub fn gc(warm_root: &Path, now: SystemTime, pressure: bool) -> GcReport {
    let mut report = GcReport::default();
    let Ok(entries) = fs::read_dir(warm_root) else {
        return report;
    };
    let mut indexes: Vec<usize> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            name.to_str()?
                .strip_prefix("slot-")?
                .strip_suffix(".lock")?
                .parse()
                .ok()
        })
        .collect();
    indexes.sort_unstable();
    let threshold = if pressure {
        PRESSURE_MIN_IDLE
    } else {
        IDLE_TTL
    };
    for index in indexes {
        let Ok(lease) = OpenOptions::new()
            .append(true)
            .open(lock_path(warm_root, index))
        else {
            continue;
        };
        if !matches!(try_lock_exclusive(&lease), Ok(true)) {
            report.in_use.push(index);
            continue;
        }
        let dir = slot_dir(warm_root, index);
        let Ok(meta) = fs::symlink_metadata(&dir) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let last_used = fs::symlink_metadata(dir.join(STATE_FILE))
            .and_then(|state| state.modified())
            .or_else(|_| meta.modified())
            .unwrap_or(UNIX_EPOCH);
        let idle = now.duration_since(last_used).unwrap_or(Duration::ZERO);
        if idle >= threshold && fs::remove_dir_all(&dir).is_ok() {
            report.removed.push(index);
        } else {
            report.kept.push(index);
        }
    }
    report
}

/// One reclaim pass over the queue's warm slots, logged. Blocking.
pub fn run_gc_and_log(trigger: &'static str, pressure: bool) {
    let root = rsi_common::identity::data_dir()
        .join("queue")
        .join("target")
        .join(WARM_DIR);
    if !root.is_dir() {
        return;
    }
    let report = gc(&root, SystemTime::now(), pressure);
    if !report.removed.is_empty() {
        tracing::info!(
            trigger,
            pressure,
            removed = report.removed.len(),
            in_use = report.in_use.len(),
            "Lander warm target slots reclaimed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("HOME", "/nonexistent")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git launch");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(path, contents).expect("file");
    }

    /// A repository with two commits: the second edits `Cargo.lock` and
    /// `crates/a/src/lib.rs` and adds `crates/b/src/extra.rs`.
    struct Repo {
        root: tempfile::TempDir,
        first: String,
        second: String,
    }

    impl Repo {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temp root");
            let repo = root.path().join("repo");
            fs::create_dir_all(&repo).expect("repo");
            git(&repo, &["init", "-q", "-b", "main"]);
            write(&repo, "Cargo.toml", "[workspace]\n");
            write(&repo, "Cargo.lock", "# lock v1\n");
            write(&repo, "crates/a/src/lib.rs", "pub fn a() {}\n");
            write(&repo, "crates/b/src/lib.rs", "pub fn b() {}\n");
            git(&repo, &["add", "."]);
            git(&repo, &["commit", "-q", "-m", "first"]);
            let first = git(&repo, &["rev-parse", "HEAD"]);
            write(&repo, "Cargo.lock", "# lock v2\n");
            write(
                &repo,
                "crates/a/src/lib.rs",
                "pub fn a() { /* changed */ }\n",
            );
            write(&repo, "crates/b/src/extra.rs", "pub fn extra() {}\n");
            git(&repo, &["add", "."]);
            git(&repo, &["commit", "-q", "-m", "second"]);
            let second = git(&repo, &["rev-parse", "HEAD"]);
            Self {
                root,
                first,
                second,
            }
        }

        fn worktree(&self, name: &str, commit: &str) -> PathBuf {
            let path = self.root.path().join(name);
            git(
                &self.root.path().join("repo"),
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    path.to_str().unwrap(),
                    commit,
                ],
            );
            path
        }
    }

    fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .expect("mtime")
    }

    fn long_ago() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_577_836_800)
    }

    /// Make every tracked file and directory of `worktree` look old, as a
    /// worktree whose build finished long ago would.
    fn age(worktree: &Path) {
        for entry in walkdir::WalkDir::new(worktree).contents_first(true) {
            let entry = entry.expect("entry");
            if entry.path().starts_with(worktree.join(".git")) || entry.path().ends_with(".git") {
                continue;
            }
            restore_mtime(entry.path(), long_ago()).expect("age");
        }
    }

    /// Model a successful build in an old checkout without a wall-clock wait.
    fn age_bound(slot: &mut WarmSlot, outcome: &BindOutcome) {
        age(&outcome.worktree);
        slot.pending = Some(stamp_worktree(&outcome.worktree, None).unwrap().0);
    }

    fn slot(root: &Path) -> WarmSlot {
        WarmSlot::acquire(&root.join(WARM_DIR), 2).expect("slot")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_warm_target_is_on_unless_the_operator_opts_out() {
        for on in [
            None,
            Some(""),
            Some("1"),
            Some("true"),
            Some("yes"),
            Some("auto"),
        ] {
            assert!(enabled_from(on), "{on:?} leaves the warm target on");
        }
        for off in [
            "0", " 0 ", "false", "FALSE", "off", "No", "disable", "Disabled",
        ] {
            assert!(!enabled_from(Some(off)), "{off:?} opts out");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn two_landers_never_lease_one_slot_and_the_lowest_free_slot_is_reused() {
        let root = tempfile::tempdir().unwrap();
        let warm = root.path().join(WARM_DIR);
        let first = WarmSlot::acquire(&warm, 2).expect("first");
        let second = WarmSlot::acquire(&warm, 2).expect("second");
        assert_eq!((first.index(), second.index()), (0, 1));
        assert!(
            WarmSlot::acquire(&warm, 2).is_none(),
            "no free slot: the gate builds cold"
        );
        assert_ne!(first.test_target(), second.test_target());
        drop(first);
        assert_eq!(WarmSlot::acquire(&warm, 2).expect("reused").index(), 0);
        assert!(WarmSlot::acquire(&warm, 0).is_none(), "0 slots disables");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_next_gate_gets_back_the_mtimes_of_files_that_did_not_change() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let mut slot = slot(root.path());
        let first = repo.worktree("gate-1", &repo.first);
        let cold = slot.bind_with_toolchain(&first, "tc").unwrap();
        age_bound(&mut slot, &cold);
        assert_eq!(cold.state, SlotState::Cold);
        assert!(!cold.manifest_found);
        assert_eq!(slot.commit().unwrap(), 4);

        let second = repo.worktree("gate-2", &repo.second);
        let warm = slot.bind_with_toolchain(&second, "tc").unwrap();
        let second = &warm.worktree;
        assert_eq!(warm.state, SlotState::Warm);
        assert!(warm.manifest_found);
        // Cargo.toml and crates/b/src/lib.rs are byte-identical across the
        // commits; Cargo.lock and crates/a/src/lib.rs changed and extra.rs is
        // new, so those three keep their checkout time.
        assert_eq!(mtime(&second.join("Cargo.toml")), long_ago());
        assert_eq!(mtime(&second.join("crates/b/src/lib.rs")), long_ago());
        assert!(mtime(&second.join("Cargo.lock")) > long_ago());
        assert!(mtime(&second.join("crates/a/src/lib.rs")) > long_ago());
        assert!(mtime(&second.join("crates/b/src/extra.rs")) > long_ago());
        assert_eq!((warm.restored_files, warm.files), (2, 5));
        // A directory gets its mtime back only while its entries (names, kinds
        // and file blob ids) are the ones the last build saw: crates/a/src
        // holds a changed file and crates/b/src gained one, but crates/a,
        // crates/b and crates only name the same children.
        assert_eq!(mtime(&second.join("crates")), long_ago());
        assert_eq!(mtime(&second.join("crates/a")), long_ago());
        assert!(mtime(&second.join("crates/a/src")) > long_ago());
        assert!(mtime(&second.join("crates/b/src")) > long_ago());
        assert!(
            mtime(&second) > long_ago(),
            "the root lists the changed Cargo.lock"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_gate_that_never_committed_leaves_the_next_gate_a_full_rebuild() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let mut slot = slot(root.path());
        let first = repo.worktree("gate-1", &repo.first);
        let bound = slot.bind_with_toolchain(&first, "tc").unwrap();
        age_bound(&mut slot, &bound);
        slot.commit().unwrap();
        // Gate 2 binds and its build fails, so it never commits.
        let second = repo.worktree("gate-2", &repo.second);
        slot.bind_with_toolchain(&second, "tc").unwrap();
        let third = repo.worktree("gate-3", &repo.first);
        let outcome = slot.bind_with_toolchain(&third, "tc").unwrap();
        assert!(!outcome.manifest_found);
        assert_eq!(outcome.restored_files, 0);
        assert!(
            mtime(&outcome.worktree.join("Cargo.toml")) > long_ago(),
            "nothing is given back without a manifest"
        );
        assert_eq!(slot.commit().unwrap(), 4, "its own build can still commit");
        assert_eq!(
            slot.commit().unwrap(),
            0,
            "a commit consumes what bind recorded"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_toolchain_change_empties_the_slot_and_a_lockfile_change_does_not() {
        // The second commit rewrites Cargo.lock.
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let mut slot = slot(root.path());
        let tree = repo.worktree("gate-1", &repo.first);
        slot.bind_with_toolchain(&tree, "rustc-1.94").unwrap();
        slot.commit().unwrap();
        write(&slot.test_target(), "debug/deps/libdep.rlib", "artifact");
        write(&slot.check_target(), "debug/deps/libdep.rmeta", "artifact");

        // Another candidate with a different Cargo.lock builds with the same
        // compiler: Cargo keys every third-party artifact on its resolved
        // package, so nothing is removed.
        let next = repo.worktree("gate-2", &repo.second);
        let kept = slot.bind_with_toolchain(&next, "rustc-1.94").unwrap();
        assert_eq!(kept.state, SlotState::Warm);
        assert!(slot.test_target().join("debug/deps/libdep.rlib").is_file());

        slot.commit().unwrap();
        let newer = repo.worktree("gate-3", &repo.second);
        let reset = slot.bind_with_toolchain(&newer, "rustc-1.95").unwrap();
        assert_eq!(reset.state, SlotState::ToolchainChanged);
        assert!(!reset.manifest_found);
        assert_eq!(reset.restored_files, 0);
        assert!(!slot.test_target().exists() && !slot.check_target().exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_oversized_slot_is_emptied_at_its_next_lease() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let mut slot = slot(root.path());
        let tree = repo.worktree("gate-1", &repo.first);
        slot.bind_with_toolchain(&tree, "tc").unwrap();
        write(
            &slot.test_target(),
            "debug/deps/big.rlib",
            &"x".repeat(4096),
        );
        slot.max_bytes = 1024;
        let outcome = slot
            .bind_with_toolchain(&repo.worktree("gate-2", &repo.second), "tc")
            .unwrap();
        assert_eq!(outcome.state, SlotState::OverSize);
        assert_eq!(outcome.bytes, 0);
        assert!(!slot.test_target().exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const DESKTOP_MANIFEST: &str = "[package]\nname = \"rsi-desktop\"\n\n[lib]\nname = \"rsi_desktop_lib\"\n\n[[bin]]\nname = \"rsi-desktop-cli\"\npath = \"src/main.rs\"\n";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_manifest_declares_the_crates_a_candidate_changes() {
        assert_eq!(
            declared_crate_names(DESKTOP_MANIFEST),
            ["rsi-desktop", "rsi-desktop-cli", "rsi_desktop_lib"]
        );
        assert!(declared_crate_names("not = [toml").is_empty());
    }

    /// Hub #3023: the lane trims the candidate's own crates from the slot's
    /// desktop target and keeps every third-party artifact, whichever spelling
    /// or directory Cargo used.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn trimming_the_desktop_target_removes_only_the_candidates_crates() {
        let root = tempfile::tempdir().unwrap();
        let slot = slot(root.path());
        let target = slot.desktop_target();
        let removed = [
            "debug/deps/rsi_desktop_lib-0123456789abcdef.d",
            "debug/deps/librsi_desktop_lib-0123456789abcdef.rlib",
            "debug/deps/rsi_desktop-0123456789abcdef",
            "debug/.fingerprint/rsi-desktop-0123456789abcdef/lib-rsi_desktop_lib",
            "debug/build/rsi-desktop-0123456789abcdef/out/gen.rs",
            "debug/incremental/rsi_desktop_lib-0123456789abcdef/s-1.lock",
            "debug/rsi-desktop",
            "debug/rsi-desktop.d",
            "debug/librsi_desktop_lib.rlib",
            "release/deps/rsi_desktop-fedcba9876543210",
        ];
        let kept = [
            "debug/deps/libtauri-0123456789abcdef.rlib",
            "debug/.fingerprint/tauri-0123456789abcdef/lib-tauri",
            "debug/build/tauri-0123456789abcdef/out/gen.rs",
            "debug/deps/librsi_desktop_extra-0123456789abcdef.rlib",
            "debug/deps/rsi_desktop_lib_helper-0123456789abcdef.d",
            "debug/deps/rsi_desktop-notahash.d",
            "debug/CACHEDIR.TAG",
        ];
        for path in removed.iter().chain(&kept) {
            write(&target, path, "artifact");
        }
        let names = declared_crate_names(DESKTOP_MANIFEST);
        assert_eq!(slot.trim_desktop(&names).unwrap(), 10);
        for path in removed {
            assert!(!target.join(path).exists(), "{path} must be trimmed");
        }
        for path in kept {
            assert!(target.join(path).exists(), "{path} must stay warm");
        }
        // Trimming again finds nothing, and no declared name empties the target.
        assert_eq!(slot.trim_desktop(&names).unwrap(), 0);
        slot.trim_desktop(&[]).unwrap();
        assert!(!target.exists());
    }

    const SENTINEL: &str = "outside the slot: must survive byte for byte";

    /// An outside tree holding a candidate-named artifact and a sentinel.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn outside_tree(root: &Path) -> PathBuf {
        let outside = root.join("outside");
        write(&outside, "librsi_common-0123456789abcdef.rlib", SENTINEL);
        write(&outside, "rsi_common-0123456789abcdef/nested", SENTINEL);
        write(&outside, "tauri-0123456789abcdef.d", SENTINEL);
        outside
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn assert_outside_untouched(outside: &Path) {
        for path in [
            "librsi_common-0123456789abcdef.rlib",
            "rsi_common-0123456789abcdef/nested",
            "tauri-0123456789abcdef.d",
        ] {
            assert_eq!(
                fs::read_to_string(outside.join(path)).expect(path),
                SENTINEL,
                "{path} outside the slot must be untouched"
            );
        }
    }

    /// #3134: a symlink where the trim needs a directory (the target root, a
    /// profile, or `deps`/`.fingerprint`/`build`/`incremental`) is never
    /// followed. The sentinel outside the slot survives, the trim does not
    /// error, and the next lane finds no warm target to reuse.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn trimming_the_desktop_target_never_follows_a_symlinked_directory() {
        use std::os::unix::fs::symlink;
        let names = vec!["rsi-common".to_owned()];
        let cases: [(&str, Option<&str>); 6] = [
            ("target root", None),
            ("profile", Some("debug")),
            ("deps", Some("debug/deps")),
            (".fingerprint", Some("debug/.fingerprint")),
            ("build", Some("debug/build")),
            ("incremental", Some("debug/incremental")),
        ];
        for (label, link) in cases {
            let root = tempfile::tempdir().unwrap();
            let slot = slot(root.path());
            let outside = outside_tree(root.path());
            let target = slot.desktop_target();
            match link {
                None => symlink(&outside, &target).unwrap(),
                Some(relative) => {
                    // Keep ordinary candidate artifacts beside the link.
                    write(
                        &target,
                        "debug/deps/librsi_common-aaaaaaaaaaaaaaaa.rlib",
                        "x",
                    );
                    let link_path = target.join(relative);
                    if let Some(parent) = link_path.parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    let _ = fs::remove_dir_all(&link_path);
                    symlink(&outside, &link_path).unwrap();
                }
            }
            slot.trim_desktop(&names)
                .unwrap_or_else(|error| panic!("{label}: {error}"));
            assert_outside_untouched(&outside);
            assert!(
                fs::symlink_metadata(&target).is_err(),
                "{label}: the next lane must start cold"
            );
            // A fresh target is usable again.
            write(
                &target,
                "debug/deps/librsi_common-bbbbbbbbbbbbbbbb.rlib",
                "y",
            );
            assert_eq!(slot.trim_desktop(&names).unwrap(), 1);
            assert_outside_untouched(&outside);
        }
    }

    /// A fake slot tree outside the lease, laid out like a slot: the artifacts
    /// the trim and the reset would remove, each holding the sentinel.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn outside_slot(root: &Path) -> PathBuf {
        let outside = root.join("outside-slot");
        for relative in [
            "desktop-target/debug/deps/librsi_common-0123456789abcdef.rlib",
            "desktop-target/debug/deps/libtauri-0123456789abcdef.rlib",
            "test-target/debug/deps/libx-0123456789abcdef.rlib",
            "check-target/marker",
            "source/Cargo.toml",
            STATE_FILE,
            MANIFEST_FILE,
        ] {
            write(&outside, relative, SENTINEL);
        }
        outside
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn assert_outside_slot_untouched(outside: &Path) {
        for relative in [
            "desktop-target/debug/deps/librsi_common-0123456789abcdef.rlib",
            "desktop-target/debug/deps/libtauri-0123456789abcdef.rlib",
            "test-target/debug/deps/libx-0123456789abcdef.rlib",
            "check-target/marker",
            "source/Cargo.toml",
            STATE_FILE,
            MANIFEST_FILE,
        ] {
            assert_eq!(
                fs::read_to_string(outside.join(relative)).expect(relative),
                SENTINEL,
                "{relative} outside the leased slot must be untouched"
            );
        }
    }

    /// Lease a slot, give it a desktop target, then rename the slot directory
    /// away and put a symlink to an outside slot at its path (#3187). Returns
    /// the slot, the renamed leased directory and the outside slot.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn slot_swapped_for_a_symlink(root: &Path) -> (WarmSlot, PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;
        let slot = slot(root);
        write(
            &slot.desktop_target(),
            "debug/deps/librsi_common-aaaaaaaaaaaaaaaa.rlib",
            "leased",
        );
        write(&slot.dir, "test-target/marker", "leased");
        write(&slot.dir, STATE_FILE, "leased");
        let outside = outside_slot(root);
        let moved = root.join("leased-moved-away");
        fs::rename(&slot.dir, &moved).unwrap();
        symlink(&outside, &slot.dir).unwrap();
        (slot, moved, outside)
    }

    /// #3187: the trim acts on the directory leased at acquisition, never on
    /// whatever the slot's pathname resolves to later.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn trimming_after_the_slot_dir_became_a_symlink_never_leaves_the_leased_dir() {
        let root = tempfile::tempdir().unwrap();
        let (slot, moved, outside) = slot_swapped_for_a_symlink(root.path());
        assert_eq!(slot.trim_desktop(&["rsi-common".to_owned()]).unwrap(), 1);
        assert_outside_slot_untouched(&outside);
        assert!(
            !moved
                .join("desktop-target/debug/deps/librsi_common-aaaaaaaaaaaaaaaa.rlib")
                .exists()
        );
    }

    /// #3187: the full-reset fallback of the trim (no usable crate names) and
    /// the slot reset both stay inside the leased directory.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_reset_after_the_slot_dir_became_a_symlink_never_leaves_the_leased_dir() {
        let root = tempfile::tempdir().unwrap();
        let (slot, moved, outside) = slot_swapped_for_a_symlink(root.path());
        slot.trim_desktop(&[]).unwrap();
        assert_outside_slot_untouched(&outside);
        assert!(!moved.join("desktop-target").exists());
        assert!(moved.join("test-target/marker").exists());

        slot.reset().unwrap();
        assert_outside_slot_untouched(&outside);
        assert!(!moved.join("test-target").exists());
        assert!(!moved.join(STATE_FILE).exists());
    }

    /// #3187: a bind into a swapped slot fails closed (the caller builds cold)
    /// and leaves the outside directory alone.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn binding_a_slot_whose_dir_became_a_symlink_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let (mut slot, _moved, outside) = slot_swapped_for_a_symlink(root.path());
        let candidate = root.path().join("candidate");
        fs::create_dir_all(&candidate).unwrap();
        assert!(slot.bind_with_toolchain(&candidate, "toolchain").is_err());
        assert_outside_slot_untouched(&outside);
    }

    /// A symlink inside a matched artifact directory is unlinked, not followed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn trimming_a_matched_directory_unlinks_symlinks_inside_it() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let slot = slot(root.path());
        let outside = outside_tree(root.path());
        let target = slot.desktop_target();
        let build = target.join("debug/build/rsi-common-0123456789abcdef");
        write(&build, "out/gen.rs", "x");
        symlink(&outside, build.join("out/escape")).unwrap();
        fs::create_dir_all(target.join("debug/deps")).unwrap();
        symlink(
            outside.join("tauri-0123456789abcdef.d"),
            target.join("debug/deps/rsi_common-0123456789abcdef.d"),
        )
        .unwrap();
        write(&target, "debug/deps/libtauri-0123456789abcdef.rlib", "keep");
        assert_eq!(slot.trim_desktop(&["rsi-common".to_owned()]).unwrap(), 2);
        assert_outside_untouched(&outside);
        assert!(!build.exists());
        assert!(
            target
                .join("debug/deps/libtauri-0123456789abcdef.rlib")
                .exists()
        );
    }

    /// A crate name that is not a plain identifier cannot select entries; it
    /// empties only the desktop target.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn odd_crate_names_only_empty_the_desktop_target() {
        for odd in ["", ".", "..", "../outside", "a/b", "rsi.common"] {
            let root = tempfile::tempdir().unwrap();
            let slot = slot(root.path());
            let outside = outside_tree(root.path());
            let target = slot.desktop_target();
            write(&target, "debug/deps/libtauri-0123456789abcdef.rlib", "x");
            write(&target, "debug/.fingerprint/p-0123456789abcdef/f", "x");
            slot.trim_desktop(&[odd.to_owned()]).unwrap();
            assert!(!target.exists(), "{odd:?}");
            assert_outside_untouched(&outside);
            assert!(root.path().join(WARM_DIR).exists(), "{odd:?}");
        }
    }

    /// A checkout whose Tauri crate reaches `rsi-graph` directly (a path
    /// dependency), `rsi-common` through `rsi-graph` and a workspace-inherited
    /// one, plus a registry crate and a path outside the checkout.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn desktop_checkout() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let manifests = [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\n[workspace.dependencies]\nrsi-extra = { path = \"crates/rsi-extra\" }\ntauri = \"2\"\n",
            ),
            (
                "desktop/src-tauri/Cargo.toml",
                "[package]\nname = \"rsi-desktop\"\n[dependencies]\ntauri = \"2\"\nrsi-graph = { path = \"../../crates/rsi-graph\" }\nrsi-extra = { workspace = true }\noutside = { path = \"../../../outside\" }\n",
            ),
            (
                "crates/rsi-graph/Cargo.toml",
                "[package]\nname = \"rsi-graph\"\n[lib]\nname = \"rsi_graph_lib\"\n[dev-dependencies]\nrsi-common = { path = \"../rsi-common\" }\n",
            ),
            (
                "crates/rsi-common/Cargo.toml",
                "[package]\nname = \"rsi-common\"\n[target.'cfg(unix)'.dependencies]\nrsi-graph = { path = \"../rsi-graph\" }\n",
            ),
            (
                "crates/rsi-extra/Cargo.toml",
                "[package]\nname = \"rsi-extra\"\n",
            ),
            (
                "crates/unrelated/Cargo.toml",
                "[package]\nname = \"unrelated\"\n",
            ),
        ];
        for (path, text) in manifests {
            write(root.path(), path, text);
        }
        root
    }

    /// Hub #3042 / #354: the trim names the Tauri crate and every workspace
    /// path-dependency crate in its closure (transitive, workspace-inherited,
    /// cycles tolerated), and nothing else.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn candidate_crates_follow_the_tauri_path_dependency_closure() {
        let checkout = desktop_checkout();
        let manifest = Path::new("desktop/src-tauri/Cargo.toml");
        assert_eq!(
            candidate_crate_names(checkout.path(), manifest),
            [
                "rsi-common",
                "rsi-desktop",
                "rsi-extra",
                "rsi-graph",
                "rsi_graph_lib"
            ]
        );
        // A path dependency whose manifest is gone cannot be named: trim it all.
        fs::remove_file(checkout.path().join("crates/rsi-common/Cargo.toml")).unwrap();
        assert!(candidate_crate_names(checkout.path(), manifest).is_empty());
        assert!(candidate_crate_names(checkout.path(), Path::new("nope/Cargo.toml")).is_empty());
    }

    /// Hub #3042 / #354: a workspace path crate's artifacts do not survive to
    /// the next candidate; the registry artifacts and the tauri crate's stay.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn trimming_the_desktop_target_drops_path_dependency_artifacts() {
        let checkout = desktop_checkout();
        let root = tempfile::tempdir().unwrap();
        let slot = slot(root.path());
        let target = slot.desktop_target();
        let removed = [
            "debug/deps/librsi_graph_lib-0123456789abcdef.rlib",
            "debug/deps/librsi_common-0123456789abcdef.rmeta",
            "debug/.fingerprint/rsi-graph-0123456789abcdef/lib-rsi_graph_lib",
            "debug/.fingerprint/rsi-common-0123456789abcdef/lib-rsi_common",
            "debug/incremental/rsi_common-0123456789abcdef/s-1.lock",
            "debug/deps/rsi_desktop-0123456789abcdef",
        ];
        let kept = [
            "debug/deps/libtauri-0123456789abcdef.rlib",
            "debug/.fingerprint/tauri-0123456789abcdef/lib-tauri",
            "debug/deps/libserde-0123456789abcdef.rlib",
            "debug/deps/librsi_graph_extra-0123456789abcdef.rlib",
        ];
        for path in removed.iter().chain(&kept) {
            write(&target, path, "artifact");
        }
        let names =
            candidate_crate_names(checkout.path(), Path::new("desktop/src-tauri/Cargo.toml"));
        assert_eq!(slot.trim_desktop(&names).unwrap(), removed.len());
        for path in removed {
            assert!(!target.join(path).exists(), "{path} must be trimmed");
        }
        for path in kept {
            assert!(target.join(path).exists(), "{path} must stay warm");
        }
    }

    /// Hub #3023: the desktop target follows the slot, so a slot's warm state
    /// and reset rules apply to it like the test and check targets.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_desktop_target_is_warm_only_while_its_slot_is() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let mut slot = slot(root.path());
        assert!(slot.desktop_target().starts_with(&slot.dir));
        assert!(
            slot.desktop_evidence()
                .starts_with("desktop_warm_target slot=0 state=cold bytes=0"),
            "{}",
            slot.desktop_evidence()
        );

        // The first bind finds an empty slot.
        slot.bind_with_toolchain(&repo.worktree("gate-1", &repo.first), "tc")
            .unwrap();
        assert!(slot.desktop_evidence().contains("state=cold"));
        write(
            &slot.desktop_target(),
            "debug/deps/libtauri-00112233.rlib",
            "tauri",
        );
        slot.commit().unwrap();

        // The next gate keeps the slot, so it keeps the desktop target.
        slot.bind_with_toolchain(&repo.worktree("gate-2", &repo.second), "tc")
            .unwrap();
        assert!(
            slot.desktop_evidence()
                .contains("slot=0 state=warm bytes=5")
        );
        assert!(
            slot.desktop_target()
                .join("debug/deps/libtauri-00112233.rlib")
                .is_file()
        );

        // A kept slot whose desktop target was never built is still cold.
        std::fs::remove_dir_all(slot.desktop_target()).unwrap();
        assert!(slot.desktop_evidence().contains("state=cold"));

        // A toolchain change empties the desktop target with the others.
        write(
            &slot.desktop_target(),
            "debug/deps/libtauri-00112233.rlib",
            "tauri",
        );
        slot.bind_with_toolchain(&repo.worktree("gate-3", &repo.second), "tc2")
            .unwrap();
        assert!(!slot.desktop_target().exists());
        assert!(slot.desktop_evidence().contains("state=toolchain_changed"));

        // The slot's size bound counts the desktop target.
        write(
            &slot.desktop_target(),
            "debug/deps/big.rlib",
            &"x".repeat(4096),
        );
        slot.max_bytes = 1024;
        let outcome = slot
            .bind_with_toolchain(&repo.worktree("gate-4", &repo.second), "tc2")
            .unwrap();
        assert_eq!(outcome.state, SlotState::OverSize);
        assert!(!slot.desktop_target().exists());
        assert!(slot.desktop_evidence().contains("state=over_size"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn every_gate_uses_the_same_clean_checkout_of_its_exact_head() {
        let repo = Repo::new();
        let root = tempfile::tempdir().unwrap();
        let mut slot = slot(root.path());
        let first = repo.worktree("gate-1", &repo.first);
        let bound = slot.bind_with_toolchain(&first, "tc").unwrap();
        age_bound(&mut slot, &bound);
        slot.commit().unwrap();
        write(&bound.worktree, "untracked-output", "old gate output");

        // A detached candidate's HEAD differs from its repository's branch tip.
        assert_eq!(git(&bound.worktree, &["rev-parse", "HEAD"]), repo.first);
        let second = repo.worktree("gate-2", &repo.second);
        let next = slot.bind_with_toolchain(&second, "tc").unwrap();
        assert_eq!(next.worktree, bound.worktree);
        assert_eq!(git(&next.worktree, &["rev-parse", "HEAD"]), repo.second);
        assert!(git(&next.worktree, &["status", "--porcelain"]).is_empty());
        assert_eq!(mtime(&next.worktree.join("Cargo.toml")), long_ago());
        assert_eq!(
            fs::read_to_string(next.worktree.join("crates/a/src/lib.rs")).unwrap(),
            "pub fn a() { /* changed */ }\n"
        );
        assert_eq!(git(&first, &["rev-parse", "HEAD"]), repo.first);
        assert!(git(&first, &["status", "--porcelain"]).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn old_format_or_relocated_slots_discard_checkout_dependent_artifacts() {
        let repo = Repo::new();
        let tree = repo.worktree("candidate", &repo.first);
        for obsolete in ["schema", "checkout"] {
            let root = tempfile::tempdir().unwrap();
            let mut slot = slot(root.path());
            let bound = slot.bind_with_toolchain(&tree, "tc").unwrap();
            age_bound(&mut slot, &bound);
            slot.commit().unwrap();
            write(&slot.test_target(), "old-artifact", "artifact");
            write(&slot.check_target(), "old-artifact", "artifact");
            let mut record: SlotRecord = read_json(&slot.dir.join(STATE_FILE)).unwrap();
            if obsolete == "schema" {
                record.schema = 1;
            } else {
                record.checkout = root.path().join("previous-location/source");
            }
            write_json(&slot.dir.join(STATE_FILE), &record).unwrap();
            let rebound = slot.bind_with_toolchain(&tree, "tc").unwrap();
            assert_eq!(rebound.state, SlotState::Cold);
            assert_eq!(rebound.restored_files, 0);
            assert!(!slot.test_target().exists() && !slot.check_target().exists());
        }
    }

    /// Dependency-free real Cargo workspace: these regressions exercise Cargo
    /// fingerprints and generated includes, not a fake command or mtime alone.
    fn cargo_repo(files: &[(&str, &str)]) -> Repo {
        cargo_repo_with_links(files, &[])
    }

    /// [`cargo_repo`] with tracked symlinks `(path, link text)`.
    fn cargo_repo_with_links(files: &[(&str, &str)], links: &[(&str, &str)]) -> Repo {
        let root = crate::test_support::disk_backed_tempdir("warm-cargo-");
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        write(
            &repo,
            "Cargo.toml",
            "[workspace]\nmembers = [\"consumer\"]\nresolver = \"2\"\n",
        );
        write(
            &repo,
            "consumer/Cargo.toml",
            "[package]\nname = \"warm-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            &repo,
            "Cargo.lock",
            "version = 4\n\n[[package]]\nname = \"warm-probe\"\nversion = \"0.1.0\"\n",
        );
        for (path, contents) in files {
            write(&repo, path, contents);
        }
        for (path, target) in links {
            let link = repo.join(path);
            fs::create_dir_all(link.parent().expect("parent")).expect("directory");
            std::os::unix::fs::symlink(target, &link).expect("symlink");
        }
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "cargo fixture"]);
        let first = git(&repo, &["rev-parse", "HEAD"]);
        Repo {
            root,
            second: first.clone(),
            first,
        }
    }

    fn cargo_test(worktree: &Path, target: &Path, test: &str) -> std::process::Output {
        cargo_test_with_build_id(worktree, target, test, None)
    }

    /// [`cargo_test`] with `RSI_BUILD_SHA` set to `build_id`, or unset.
    fn cargo_test_with_build_id(
        worktree: &Path,
        target: &Path,
        test: &str,
        build_id: Option<&str>,
    ) -> std::process::Output {
        let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        command
            .current_dir(worktree)
            .args([
                "test",
                "--offline",
                "--locked",
                "--lib",
                "--verbose",
                test,
                "--",
                "--exact",
            ])
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_BUILD_JOBS", "1");
        match build_id {
            Some(id) => command.env("RSI_BUILD_SHA", id),
            None => command.env_remove("RSI_BUILD_SHA"),
        };
        command
            .stdin(Stdio::null())
            .output()
            .expect("real cargo launch")
    }

    fn assert_cargo_passed(output: &std::process::Output) {
        assert!(
            output.status.success(),
            "cargo stdout:\n{}\ncargo stderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn real_cargo_rebuilds_an_external_include_while_the_old_candidate_still_exists() {
        let test = "embedded_asset_is_current";
        let mut repo = cargo_repo(&[
            ("remote/web/index.html", "clean\n"),
            (
                "consumer/src/lib.rs",
                "const PAGE: &str = include_str!(\"../../remote/web/index.html\");\n\
                 #[test] fn embedded_asset_is_current() { assert_eq!(PAGE, \"clean\\n\"); }\n",
            ),
        ]);
        let repository = repo.root.path().join("repo");
        write(&repository, "remote/web/index.html", "<style>new</style>\n");
        git(&repository, &["add", "remote/web/index.html"]);
        git(
            &repository,
            &["commit", "-q", "-m", "changed external asset"],
        );
        repo.second = git(&repository, &["rev-parse", "HEAD"]);
        let mut slot = slot(repo.root.path());
        let first = repo.worktree("gate-a", &repo.first);
        let bound = slot.bind_with_toolchain(&first, "tc").unwrap();
        age_bound(&mut slot, &bound);
        assert_cargo_passed(&cargo_test(&bound.worktree, &slot.test_target(), test));
        slot.commit().unwrap();
        // A genuinely fresh unchanged build must be reusable too.
        let unchanged = slot.bind_with_toolchain(&first, "tc").unwrap();
        let fresh = cargo_test(&unchanged.worktree, &slot.test_target(), test);
        assert_cargo_passed(&fresh);
        assert!(String::from_utf8_lossy(&fresh.stderr).contains("Fresh warm-probe"));
        slot.commit().unwrap();

        let second = repo.worktree("gate-b", &repo.second);
        let rebound = slot.bind_with_toolchain(&second, "tc").unwrap();
        assert_eq!(bound.worktree, rebound.worktree);
        assert_eq!(
            mtime(&rebound.worktree.join("consumer/src/lib.rs")),
            long_ago()
        );
        assert_eq!(
            fs::read_to_string(first.join("remote/web/index.html")).unwrap(),
            "clean\n"
        );
        let warm = cargo_test(&rebound.worktree, &slot.test_target(), test);
        let cold = cargo_test(&second, &repo.root.path().join("cold-target"), test);
        for output in [warm, cold] {
            assert_eq!(output.status.code(), Some(101));
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("test embedded_asset_is_current ... FAILED"),
                "expected the embedded asset assertion: {output:?}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn real_cargo_reuses_out_dir_relative_includes_after_the_old_candidate_is_deleted() {
        let test = "generated_input_is_current";
        let lib = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n\
                   #[test] fn generated_input_is_current() { assert_eq!(value(), 7); }\n";
        let mut repo = cargo_repo(&[
            ("consumer/src/lib.rs", lib),
            (
                "consumer/src/generated_input.rs",
                "pub fn value() -> u8 { 7 }\n",
            ),
            (
                "consumer/build.rs",
                r#"use std::{env, fs, path::PathBuf};
fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let input = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("src/generated_input.rs");
    let mut base = out.as_path();
    let mut relative = PathBuf::new();
    while !input.starts_with(base) {
        relative.push("..");
        base = base.parent().unwrap();
    }
    relative.push(input.strip_prefix(base).unwrap());
    fs::write(out.join("generated.rs"), format!("include!({:?});\n", relative.to_str().unwrap())).unwrap();
    println!("cargo:rerun-if-changed=src/generated_input.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
"#,
            ),
        ]);
        let repository = repo.root.path().join("repo");
        write(
            &repository,
            "consumer/src/lib.rs",
            &format!("{lib}// force library recompilation\n"),
        );
        git(&repository, &["add", "consumer/src/lib.rs"]);
        git(
            &repository,
            &["commit", "-q", "-m", "changed consumer only"],
        );
        repo.second = git(&repository, &["rev-parse", "HEAD"]);
        let warm_root = repo.root.path().join(WARM_DIR);
        let mut slot = WarmSlot::acquire(&warm_root, 1).unwrap();
        let first = repo.worktree("gate-a", &repo.first);
        let bound = slot.bind_with_toolchain(&first, "tc").unwrap();
        age_bound(&mut slot, &bound);
        assert_cargo_passed(&cargo_test(&bound.worktree, &slot.test_target(), test));
        slot.commit().unwrap();
        let generated = walkdir::WalkDir::new(slot.test_target())
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name() == "generated.rs")
            .unwrap()
            .into_path();
        let generated_mtime = mtime(&generated);
        let generated_contents = fs::read_to_string(&generated).unwrap();
        assert!(generated_contents.contains("../"));
        git(
            &repository,
            &["worktree", "remove", first.to_str().unwrap()],
        );
        drop(slot);

        // A new lease represents another lander; its build script is unchanged
        // but the library must compile using the existing generated include.
        let mut slot = WarmSlot::acquire(&warm_root, 1).unwrap();
        let second = repo.worktree("gate-b", &repo.second);
        let rebound = slot.bind_with_toolchain(&second, "tc").unwrap();
        assert_eq!(bound.worktree, rebound.worktree);
        assert_eq!(
            mtime(&rebound.worktree.join("consumer/build.rs")),
            long_ago()
        );
        let warm = cargo_test(&rebound.worktree, &slot.test_target(), test);
        assert_cargo_passed(&warm);
        assert!(String::from_utf8_lossy(&warm.stderr).contains("Compiling warm-probe"));
        assert_eq!(
            mtime(&generated),
            generated_mtime,
            "the build script stayed fresh"
        );
        assert_eq!(fs::read_to_string(&generated).unwrap(), generated_contents);
        assert_cargo_passed(&cargo_test(
            &second,
            &repo.root.path().join("cold-target"),
            test,
        ));
    }

    /// Gate A builds `repo.first` and passes `test`. Gate B reuses the slot for
    /// `repo.second`, whose only change is to the bytes the test reads without
    /// touching any regular file's blob id (#2508): it must fail, as a cold
    /// build of the same commit does. Returns gate B's bind.
    fn assert_changed_bytes_fail_warm_and_cold(repo: &Repo, test: &str) -> BindOutcome {
        let mut slot = slot(repo.root.path());
        let first = repo.worktree("gate-a", &repo.first);
        let bound = slot.bind_with_toolchain(&first, "tc").unwrap();
        age_bound(&mut slot, &bound);
        assert_cargo_passed(&cargo_test(&bound.worktree, &slot.test_target(), test));
        slot.commit().unwrap();

        let second = repo.worktree("gate-b", &repo.second);
        let rebound = slot.bind_with_toolchain(&second, "tc").unwrap();
        assert_eq!(bound.worktree, rebound.worktree);
        let warm = cargo_test(&rebound.worktree, &slot.test_target(), test);
        let cold = cargo_test(&second, &repo.root.path().join("cold-target"), test);
        for output in [warm, cold] {
            assert_eq!(output.status.code(), Some(101), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains(&format!("test {test} ... FAILED")),
                "expected the assertion on the changed bytes: {output:?}"
            );
        }
        rebound
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn real_cargo_rebuilds_an_include_whose_symlink_was_retargeted() {
        let mut repo = cargo_repo_with_links(
            &[
                ("assets/a.txt", "a\n"),
                ("assets/b.txt", "b\n"),
                (
                    "consumer/src/lib.rs",
                    "const PAGE: &str = include_str!(\"../../assets/page.txt\");\n\
                     #[test] fn page_is_a() { assert_eq!(PAGE, \"a\\n\"); }\n",
                ),
            ],
            &[("assets/page.txt", "a.txt")],
        );
        // Both targets keep their own blob id and mtime; only the link moves.
        let repository = repo.root.path().join("repo");
        fs::remove_file(repository.join("assets/page.txt")).unwrap();
        std::os::unix::fs::symlink("b.txt", repository.join("assets/page.txt")).unwrap();
        git(&repository, &["add", "assets/page.txt"]);
        git(&repository, &["commit", "-q", "-m", "retarget the link"]);
        repo.second = git(&repository, &["rev-parse", "HEAD"]);

        let rebound = assert_changed_bytes_fail_warm_and_cold(&repo, "page_is_a");
        assert!(rebound.manifest_found && rebound.checkout_changed);
        assert_eq!((rebound.restored_files, rebound.restored_dirs), (0, 0));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn real_cargo_rebuilds_an_include_when_only_gitattributes_changed() {
        let mut repo = cargo_repo(&[
            ("assets/page.txt", "a\n"),
            (
                "consumer/src/lib.rs",
                "const PAGE: &str = include_str!(\"../../assets/page.txt\");\n\
                 #[test] fn page_has_no_carriage_return() { assert!(!PAGE.contains('\\r')); }\n",
            ),
        ]);
        // The blob of page.txt stays the same; checkout now writes `a\r\n`.
        let repository = repo.root.path().join("repo");
        write(
            &repository,
            ".gitattributes",
            "assets/*.txt text eol=crlf\n",
        );
        git(&repository, &["add", ".gitattributes"]);
        git(&repository, &["commit", "-q", "-m", "convert the asset"]);
        repo.second = git(&repository, &["rev-parse", "HEAD"]);

        let rebound = assert_changed_bytes_fail_warm_and_cold(&repo, "page_has_no_carriage_return");
        assert_eq!(
            fs::read(rebound.worktree.join("assets/page.txt")).unwrap(),
            b"a\r\n",
            "the fixture must exercise a checkout conversion"
        );
        assert!(rebound.manifest_found && rebound.checkout_changed);
        assert_eq!((rebound.restored_files, rebound.restored_dirs), (0, 0));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn symlinks_and_attributes_decide_whether_a_gate_gets_mtimes_back() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repo");
        fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        write(&repository, "assets/a.txt", "a\n");
        write(&repository, "assets/b.txt", "b\n");
        write(&repository, "src/lib.rs", "pub fn f() {}\n");
        std::os::unix::fs::symlink("a.txt", repository.join("assets/page.txt")).unwrap();
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-q", "-m", "link to a"]);
        let linked_a = git(&repository, &["rev-parse", "HEAD"]);
        fs::remove_file(repository.join("assets/page.txt")).unwrap();
        std::os::unix::fs::symlink("b.txt", repository.join("assets/page.txt")).unwrap();
        git(&repository, &["add", "assets/page.txt"]);
        git(&repository, &["commit", "-q", "-m", "link to b"]);
        let linked_b = git(&repository, &["rev-parse", "HEAD"]);
        write(&repository, ".gitattributes", "*.txt text eol=crlf\n");
        git(&repository, &["add", ".gitattributes"]);
        git(&repository, &["commit", "-q", "-m", "attributes"]);
        let attributed = git(&repository, &["rev-parse", "HEAD"]);
        let tree = |name: &str, commit: &str| {
            let path = root.path().join(name);
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    path.to_str().unwrap(),
                    commit,
                ],
            );
            path
        };

        let mut slot = slot(root.path());
        let bound = slot
            .bind_with_toolchain(&tree("gate-1", &linked_a), "tc")
            .unwrap();
        age_bound(&mut slot, &bound);
        slot.commit().unwrap();

        // A retargeted link: the manifest is found and gives nothing back.
        let retargeted = slot
            .bind_with_toolchain(&tree("gate-2", &linked_b), "tc")
            .unwrap();
        assert!(retargeted.manifest_found && retargeted.checkout_changed);
        assert_eq!(
            (retargeted.restored_files, retargeted.restored_dirs),
            (0, 0)
        );
        assert!(mtime(&retargeted.worktree.join("src/lib.rs")) > long_ago());
        age_bound(&mut slot, &retargeted);
        slot.commit().unwrap();

        // A new .gitattributes does the same.
        let converted = slot
            .bind_with_toolchain(&tree("gate-3", &attributed), "tc")
            .unwrap();
        assert!(converted.manifest_found && converted.checkout_changed);
        assert_eq!((converted.restored_files, converted.restored_dirs), (0, 0));
        age_bound(&mut slot, &converted);
        slot.commit().unwrap();

        // The same links and attributes again: everything is given back.
        let unchanged = slot
            .bind_with_toolchain(&tree("gate-4", &attributed), "tc")
            .unwrap();
        assert!(unchanged.manifest_found && !unchanged.checkout_changed);
        assert_eq!(
            (unchanged.restored_files, unchanged.files),
            (4, 4),
            "a.txt, b.txt, src/lib.rs and .gitattributes"
        );
        assert_eq!(mtime(&unchanged.worktree.join("src/lib.rs")), long_ago());
        assert!(unchanged.evidence(0).ends_with("checkout_changed=false"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn checkout_settings_follow_the_conversion_config_and_attribute_files() {
        let repo = Repo::new();
        let tree = repo.worktree("gate-1", &repo.first);
        let repository = repo.root.path().join("repo");
        let base = checkout_settings(&tree).unwrap();
        assert_eq!(checkout_settings(&tree).unwrap(), base, "deterministic");

        git(&repository, &["config", "core.autocrlf", "true"]);
        assert_ne!(checkout_settings(&tree).unwrap(), base);
        git(&repository, &["config", "--unset", "core.autocrlf"]);
        assert_eq!(checkout_settings(&tree).unwrap(), base);

        git(&repository, &["config", "filter.fix.smudge", "cat"]);
        assert_ne!(checkout_settings(&tree).unwrap(), base);
        git(&repository, &["config", "--unset", "filter.fix.smudge"]);
        assert_eq!(checkout_settings(&tree).unwrap(), base);

        write(&repository, ".git/info/attributes", "*.rs eol=crlf\n");
        assert_ne!(checkout_settings(&tree).unwrap(), base);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_checkout_that_is_not_the_candidates_head_is_refused() {
        let repo = Repo::new();
        let first = repo.worktree("gate-1", &repo.first);
        assert_eq!(head_commit(&first).unwrap(), repo.first);
        ensure_head(&first, &repo.first).unwrap();
        let error = ensure_head(&first, &repo.second).unwrap_err().to_string();
        assert!(
            error.contains(&repo.first) && error.contains(&repo.second),
            "{error}"
        );

        // A candidate without a HEAD commit cannot be bound at all: the caller
        // then builds cold, and nothing is left to commit.
        let root = tempfile::tempdir().unwrap();
        let mut slot = slot(root.path());
        let unborn = root.path().join("unborn");
        fs::create_dir(&unborn).unwrap();
        git(&unborn, &["init", "-q", "-b", "main"]);
        assert!(slot.bind_with_toolchain(&unborn, "tc").is_err());
        assert_eq!(slot.commit().unwrap(), 0);
    }

    /// The build-id code of the real `crates/rsid/build.rs` with a trivial
    /// `main`, so the regression below runs the code that ships.
    fn gate_build_script() -> String {
        let real =
            fs::read_to_string(Path::new(rsi_common::manifest_dir!()).join("build.rs")).unwrap();
        let (build_id, _) = real
            .split_once("// SEAM-GUARD-BEGIN")
            .expect("build.rs keeps its seam guard marker");
        format!(
            "{build_id}fn main() {{\n    emit_build_sha();\n    emit_build_main_worktree();\n    \
             println!(\"cargo:rerun-if-changed=build.rs\");\n}}\n"
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn real_cargo_keeps_rsid_fresh_across_candidates_only_when_the_build_id_is_pinned() {
        let test = "build_id_is_set";
        let script = gate_build_script();
        let mut repo = cargo_repo(&[
            ("consumer/build.rs", script.as_str()),
            (
                "consumer/src/lib.rs",
                "pub const ID: &str = env!(\"RSI_BUILD_SHA\");\n\
                 #[test] fn build_id_is_set() { assert!(!ID.is_empty()); }\n",
            ),
        ]);
        // The next candidate changes nothing the crate reads.
        let repository = repo.root.path().join("repo");
        write(&repository, "notes.txt", "unrelated\n");
        git(&repository, &["add", "notes.txt"]);
        git(&repository, &["commit", "-q", "-m", "unrelated file"]);
        repo.second = git(&repository, &["rev-parse", "HEAD"]);

        // Builds a candidate in a fresh slot, rebinds the slot to the next
        // candidate and returns what Cargo said about the second build.
        let second_build = |label: &str, build_id: Option<&str>| -> String {
            let home = repo.root.path().join(label);
            fs::create_dir(&home).unwrap();
            let mut slot = slot(&home);
            let first = repo.worktree(&format!("{label}-a"), &repo.first);
            let bound = slot.bind_with_toolchain(&first, "tc").unwrap();
            age_bound(&mut slot, &bound);
            let target = slot.test_target();
            assert_cargo_passed(&cargo_test_with_build_id(
                &bound.worktree,
                &target,
                test,
                build_id,
            ));
            slot.commit().unwrap();

            let second = repo.worktree(&format!("{label}-b"), &repo.second);
            let rebound = slot.bind_with_toolchain(&second, "tc").unwrap();
            assert_eq!(
                (rebound.restored_files, rebound.files),
                (5, 6),
                "every file but the new notes.txt keeps its mtime"
            );
            let output = cargo_test_with_build_id(&rebound.worktree, &target, test, build_id);
            assert_cargo_passed(&output);
            String::from_utf8_lossy(&output.stderr).into_owned()
        };

        // Gate builds pin the id: the build script does not rerun, so the
        // crate (and in rsid's case everything above it) stays Fresh.
        let pinned = second_build("pinned", Some("gate-build"));
        assert!(pinned.contains("Fresh warm-probe"), "{pinned}");
        assert!(!pinned.contains("Compiling warm-probe"), "{pinned}");
        // Without a pinned id the script reads the clone's HEAD, which a new
        // bind rewrites, so the same unchanged crate recompiles.
        let from_git = second_build("from-git", None);
        assert!(from_git.contains("Compiling warm-probe"), "{from_git}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn reclaim_skips_leased_slots_and_removes_idle_ones() {
        let root = tempfile::tempdir().unwrap();
        let warm = root.path().join(WARM_DIR);
        let busy = WarmSlot::acquire(&warm, 2).expect("busy");
        let idle = WarmSlot::acquire(&warm, 2).expect("idle");
        write(&busy.test_target(), "debug/a", "busy");
        write(&idle.test_target(), "debug/a", "idle");
        let idle_index = idle.index();
        drop(idle);
        let state = slot_dir(&warm, idle_index).join(STATE_FILE);
        fs::write(&state, "{}").unwrap();
        let now = SystemTime::now();

        // A recent slot survives an ordinary pass.
        let report = gc(&warm, now, false);
        assert_eq!(report.removed, Vec::<usize>::new());
        assert_eq!(report.in_use, vec![busy.index()]);
        assert_eq!(report.kept, vec![idle_index]);

        // Idle for ten minutes: only disk pressure reclaims it.
        let later = now + PRESSURE_MIN_IDLE + Duration::from_secs(1);
        assert_eq!(gc(&warm, later, false).removed, Vec::<usize>::new());
        let report = gc(&warm, later, true);
        assert_eq!(report.removed, vec![idle_index]);
        assert_eq!(report.in_use, vec![busy.index()]);
        assert!(!slot_dir(&warm, idle_index).exists());
        assert!(busy.test_target().join("debug/a").is_file());

        // Past the idle TTL an ordinary pass reclaims an unleased slot too.
        let index = busy.index();
        drop(busy);
        let report = gc(&warm, now + IDLE_TTL + Duration::from_secs(1), false);
        assert_eq!(report.removed, vec![index]);
    }
}
