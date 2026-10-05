//! Trusted allocation custody for scratch reclaim (#1140).
//!
//! Provenance is not a marker an arbitrary file write can produce. Every
//! recorded scratch directory has an allocation entry in a private registry
//! under the daemon's data directory (`~/.rsi/scratch-registry`, mode 0700,
//! entries 0600), written when the directory was created, binding its kind,
//! owner, device, inode, filesystem birth time and parent directory. The
//! in-tree record carries only the nonce that names the entry. A record that
//! was copied (new inode and birth time), moved to another parent, replayed
//! after inode reuse (new birth time), or synthesized without an entry never
//! binds, and the directory is retained.
//!
//! The registry also persists the proved deletion manifest of a reclaim in
//! progress, outside the tree being deleted, so a resumed removal can only
//! delete what the final proof saw.
//!
//! Trust boundary: the registry lives where only the daemon user can write. A
//! process running as that same user and set on forging provenance can write
//! it too; no file a same-user process can rewrite defends against that. The
//! guarantee is against everything else: copies, moves, stale or replayed
//! records, look-alike names, other tools' directories, and records written by
//! mistake.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::fsys::{self, Budget, Ident, Pinned};

const REGISTRY_DIR_NAME: &str = "scratch-registry";
const ENTRY_MAGIC: &str = "rsi-scratch-alloc v1";
const ENTRY_LIMIT: usize = 16 * 1024;
const MANIFEST_MAGIC: &[u8] = b"rsi-scratch-manifest v1\n";
const MANIFEST_LIMIT: usize = 128 << 20;
const MANIFEST_SUFFIX: &str = ".manifest";
/// Entries untouched this long whose directory is gone are pruned.
const STALE_ENTRY_SECS: u64 = 24 * 3600;

/// `~/.rsi/scratch-registry`.
pub(super) fn default_registry_path() -> PathBuf {
    rsi_common::identity::data_dir().join(REGISTRY_DIR_NAME)
}

/// A nonce names one allocation: 32 lowercase hex digits.
pub(super) fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn new_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The exact private clone a lander registered inside its workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CloneBinding {
    pub ident: Ident,
    /// Birth time in nanoseconds since the epoch.
    pub btime: i128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub nonce: String,
    pub kind: String,
    pub uid: u32,
    pub ident: Ident,
    /// Birth time in nanoseconds since the epoch.
    pub btime: i128,
    /// The directory the allocation was made in (the root it must stay in).
    pub parent: Ident,
    pub created_unix: u64,
    /// Informational: where it was made.
    pub path: String,
    /// Lander only: the sandbox the workspace was made for.
    pub source: Option<String>,
    /// Lander only: the exact private clone.
    pub clone: Option<CloneBinding>,
}

impl Entry {
    pub(super) fn to_text(&self) -> String {
        let mut text = format!(
            "{ENTRY_MAGIC}\nnonce={}\nkind={}\nuid={}\ndev={}\nino={}\nbtime_ns={}\nparent_dev={}\nparent_ino={}\ncreated_unix={}\n",
            self.nonce,
            self.kind,
            self.uid,
            self.ident.dev,
            self.ident.ino,
            self.btime,
            self.parent.dev,
            self.parent.ino,
            self.created_unix,
        );
        if let Some(clone) = self.clone {
            text.push_str(&format!(
                "clone_dev={}\nclone_ino={}\nclone_btime_ns={}\n",
                clone.ident.dev, clone.ident.ino, clone.btime
            ));
        }
        if let Some(source) = &self.source {
            text.push_str(&format!("source={}\n", one_line(source)));
        }
        text.push_str(&format!("path={}\n", one_line(&self.path)));
        text
    }

    pub(super) fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != ENTRY_MAGIC {
            return None;
        }
        let (mut nonce, mut kind, mut uid, mut dev, mut ino) = (None, None, None, None, None);
        let (mut btime, mut pdev, mut pino, mut created) = (None, None, None, None);
        let (mut cdev, mut cino, mut cbtime) = (None, None, None);
        let (mut source, mut path) = (None, String::new());
        for line in lines {
            let (key, value) = line.split_once('=')?;
            match key {
                "nonce" => nonce = Some(value.to_string()),
                "kind" => kind = Some(value.to_string()),
                "uid" => uid = value.parse::<u32>().ok(),
                "dev" => dev = value.parse::<u64>().ok(),
                "ino" => ino = value.parse::<u64>().ok(),
                "btime_ns" => btime = value.parse::<i128>().ok(),
                "parent_dev" => pdev = value.parse::<u64>().ok(),
                "parent_ino" => pino = value.parse::<u64>().ok(),
                "created_unix" => created = value.parse::<u64>().ok(),
                "clone_dev" => cdev = value.parse::<u64>().ok(),
                "clone_ino" => cino = value.parse::<u64>().ok(),
                "clone_btime_ns" => cbtime = value.parse::<i128>().ok(),
                "source" => source = Some(value.to_string()),
                "path" => path = value.to_string(),
                _ => {}
            }
        }
        let nonce = nonce.filter(|n| valid_nonce(n))?;
        let clone = match (cdev, cino, cbtime) {
            (Some(dev), Some(ino), Some(btime)) => Some(CloneBinding {
                ident: Ident { dev, ino },
                btime,
            }),
            (None, None, None) => None,
            // A half-written clone binding is not a binding.
            _ => return None,
        };
        Some(Self {
            nonce,
            kind: kind?,
            uid: uid?,
            ident: Ident {
                dev: dev?,
                ino: ino?,
            },
            btime: btime?,
            parent: Ident {
                dev: pdev?,
                ino: pino?,
            },
            created_unix: created?,
            path,
            source,
            clone,
        })
    }

    /// Whether this allocation is the directory `dir`, of `kind`, owned by
    /// `uid`, still in the root `parent` it was made in.
    pub(super) fn binds(&self, dir: &Pinned, parent: &Pinned, kind: &str, uid: u32) -> bool {
        self.kind == kind
            && self.uid == uid
            && dir.uid == uid
            && self.ident == dir.ident
            && dir.btime == Some(self.btime)
            && self.parent == parent.ident
    }
}

fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// What the registry holds for a reclaim's persisted deletion manifest.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ManifestState {
    /// No manifest file: no earlier pass published a final proof.
    Absent,
    Present(Vec<u64>),
    /// A manifest exists (or its state cannot be read) but is not usable.
    Invalid,
}

fn parse_manifest(data: &[u8]) -> Option<Vec<u64>> {
    let body = data.strip_prefix(MANIFEST_MAGIC)?;
    let (count, rest) = body.split_at_checked(8)?;
    let count = usize::try_from(u64::from_le_bytes(count.try_into().ok()?)).ok()?;
    if rest.len() != count.checked_mul(8)? {
        return None;
    }
    let hashes: Vec<u64> = rest
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])))
        .collect();
    // Membership is a binary search: refuse an unsorted list.
    hashes.windows(2).all(|w| w[0] < w[1]).then_some(hashes)
}

/// The private registry directory.
pub(super) struct Registry {
    dir: Pinned,
    uid: u32,
}

impl Registry {
    /// Open the registry: an authenticated path, a directory of ours that no
    /// one else can write.
    pub(super) fn open(path: &Path, uid: u32) -> io::Result<Self> {
        let dir = fsys::open_root(path, uid)?;
        if dir.uid != uid || dir.mode & 0o077 != 0 {
            return Err(io::Error::other("scratch registry is not private"));
        }
        Ok(Self { dir, uid })
    }

    /// Open the registry, creating it (mode 0700) when absent.
    pub(super) fn open_or_create(path: &Path, uid: u32) -> io::Result<Self> {
        match Self::open(path, uid) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new().mode(0o700).create(path)?;
                Self::open(path, uid)
            }
            other => other,
        }
    }

    pub(super) fn get(&self, nonce: &str) -> Option<Entry> {
        if !valid_nonce(nonce) {
            return None;
        }
        let (data, stat) = fsys::read_file_at(self.dir.raw(), nonce, ENTRY_LIMIT, Some(self.uid))
            .ok()
            .flatten()?;
        // Another user must not have been able to write it.
        if fsys::file_mode(&stat) & 0o022 != 0 {
            return None;
        }
        let entry = Entry::parse(std::str::from_utf8(&data).ok()?)?;
        (entry.nonce == nonce).then_some(entry)
    }

    pub(super) fn put_new(&self, entry: &Entry) -> io::Result<()> {
        fsys::create_file(&self.dir, &entry.nonce, entry.to_text().as_bytes())
    }

    pub(super) fn replace(&self, entry: &Entry) -> io::Result<()> {
        fsys::replace_file(&self.dir, &entry.nonce, entry.to_text().as_bytes())
    }

    pub(super) fn remove_entry(&self, nonce: &str) {
        if valid_nonce(nonce) {
            fsys::remove_file_quiet(&self.dir, nonce);
        }
    }

    fn manifest_name(nonce: &str) -> String {
        format!("{nonce}{MANIFEST_SUFFIX}")
    }

    /// The persisted deletion manifest of `nonce`. Only a file that does not
    /// exist is `Absent`; one that cannot be read, is not ours, is writable by
    /// others, or does not parse as a sorted hash list is `Invalid`, and an
    /// invalid proof is never treated as no proof.
    pub(super) fn manifest_get(&self, nonce: &str) -> ManifestState {
        if !valid_nonce(nonce) {
            return ManifestState::Invalid;
        }
        let read = fsys::read_file_at(
            self.dir.raw(),
            &Self::manifest_name(nonce),
            MANIFEST_LIMIT,
            Some(self.uid),
        );
        let (data, stat) = match read {
            Ok(Some(found)) => found,
            Ok(None) => return ManifestState::Absent,
            Err(_) => return ManifestState::Invalid,
        };
        if fsys::file_mode(&stat) & 0o022 != 0 {
            return ManifestState::Invalid;
        }
        parse_manifest(&data).map_or(ManifestState::Invalid, ManifestState::Present)
    }

    pub(super) fn manifest_put(&self, nonce: &str, hashes: &[u64]) -> io::Result<()> {
        if !valid_nonce(nonce) {
            return Err(io::Error::other("invalid scratch nonce"));
        }
        let mut data = Vec::with_capacity(MANIFEST_MAGIC.len() + 8 + hashes.len() * 8);
        data.extend_from_slice(MANIFEST_MAGIC);
        data.extend_from_slice(&(hashes.len() as u64).to_le_bytes());
        for hash in hashes {
            data.extend_from_slice(&hash.to_le_bytes());
        }
        fsys::replace_file(&self.dir, &Self::manifest_name(nonce), &data)
    }

    pub(super) fn manifest_remove(&self, nonce: &str) {
        if valid_nonce(nonce) {
            fsys::remove_file_quiet(&self.dir, &Self::manifest_name(nonce));
        }
    }

    /// Take the custody lock for one reclaim, shared with other reclaims and
    /// exclusive of a prune. It is held from the rename aside to the removal of
    /// the allocation's entry and manifest, so a prune never runs against a
    /// reclaim's half-moved tree. Never waits: `None` means a prune holds the
    /// lock and the candidate is deferred to a later pass.
    pub(super) fn reclaim_guard(&self) -> io::Result<Option<CustodyGuard>> {
        self.custody(false)
    }

    /// The custody lock held for a prune, or `None` when a reclaim (or another
    /// prune) holds it or it cannot be taken: pruning then waits for a later pass.
    fn prune_guard(&self) -> Option<CustodyGuard> {
        self.custody(true).ok().flatten()
    }

    /// `None` when a conflicting hold exists.
    pub(super) fn custody(&self, exclusive: bool) -> io::Result<Option<CustodyGuard>> {
        let file = fsys::open_dir_lock(&self.dir)?;
        Ok(fsys::try_flock(&file, exclusive)?.then_some(CustodyGuard { _file: file }))
    }

    /// Drop entries (and manifests) whose allocation is provably gone, at most
    /// `max` names per call and within the pass budget, serialized with every
    /// reclaim by the custody lock (a busy lock prunes nothing this pass).
    ///
    /// An allocation is dropped only when its directory is proved absent from
    /// its authenticated registered parent: the parent is opened component by
    /// component without a symlink and must be the very directory the entry
    /// registered, every name in it is inventoried twice, and none of them is
    /// the allocation's device, inode and birth time (so a rename within the
    /// parent, to any name, keeps it). Anything that cannot be proved - a
    /// parent that moved, cannot be opened or listed, a stat error, an
    /// exhausted budget, a manifest of a reclaim, a live aside name - keeps the
    /// entry.
    pub(super) fn prune(
        &self,
        now_unix: u64,
        max: usize,
        budget: &mut Budget,
        mounts: Option<&[PathBuf]>,
    ) -> usize {
        let Some(_custody) = self.prune_guard() else {
            return 0;
        };
        let Ok(names) = fsys::entry_names(self.dir.raw(), true, budget) else {
            return 0;
        };
        let mut pruned = 0;
        let mut parents: HashMap<PathBuf, Option<ParentView>> = HashMap::new();
        for name in names.into_iter().take(max) {
            let (nonce, is_manifest) = match name.strip_suffix(MANIFEST_SUFFIX) {
                Some(nonce) => (nonce, true),
                None => (name.as_str(), false),
            };
            if !valid_nonce(nonce) {
                continue;
            }
            if is_manifest {
                // A manifest whose entry is provably absent is unreachable.
                if matches!(fsys::stat_opt(&self.dir, nonce), Ok(None)) {
                    fsys::remove_file_quiet(&self.dir, &name);
                    pruned += 1;
                }
                continue;
            }
            let Some(entry) = self.get(nonce) else {
                continue;
            };
            if entry.uid != self.uid
                || now_unix <= entry.created_unix.saturating_add(STALE_ENTRY_SECS)
            {
                continue;
            }
            // A reclaim in progress has a manifest; an unreadable state keeps it.
            if !self.manifest_is_absent(nonce) {
                continue;
            }
            if !self.allocation_absent(&entry, &mut parents, budget, mounts) {
                continue;
            }
            // The cached view may be stale by now (a name moved out and back
            // since it was taken). Prove absence again from a fresh view of the
            // parent, immediately before the drop, under the same lock.
            if budget.expired() {
                break;
            }
            let fresh = Path::new(&entry.path)
                .parent()
                .and_then(|parent| ParentView::open(parent, self.uid, budget, mounts));
            if !Self::absent_in(fresh.as_ref(), &entry) || budget.expired() {
                continue;
            }
            // Re-read under the lock just before the drop.
            //
            // Residual, stated plainly: a rename that lands after this last
            // check and before the `unlinkat` below cannot be excluded without
            // kernel support (the registry file and the scratch tree are in
            // different directories and no primitive makes the pair atomic).
            // Its cost is bounded to registry-only loss: the entry is dropped,
            // the tree is kept and can no longer bind. No scratch data is ever
            // removed here. The same bound covers an adversarial same-user
            // process, which the registry's trust boundary already excludes.
            if self.manifest_is_absent(nonce) {
                self.remove_entry(nonce);
                pruned += 1;
            }
        }
        pruned
    }

    fn manifest_is_absent(&self, nonce: &str) -> bool {
        matches!(
            fsys::stat_opt(&self.dir, &Self::manifest_name(nonce)),
            Ok(None)
        )
    }

    /// Whether `entry`'s directory is proved absent from the parent it was
    /// registered in (see [`Registry::prune`]).
    fn allocation_absent(
        &self,
        entry: &Entry,
        parents: &mut HashMap<PathBuf, Option<ParentView>>,
        budget: &mut Budget,
        mounts: Option<&[PathBuf]>,
    ) -> bool {
        let Some(parent_path) = Path::new(&entry.path).parent() else {
            return false;
        };
        let view = parents
            .entry(parent_path.to_path_buf())
            .or_insert_with(|| ParentView::open(parent_path, self.uid, budget, mounts));
        Self::absent_in(view.as_ref(), entry)
    }

    /// Whether `view` proves `entry` absent from its registered parent.
    fn absent_in(view: Option<&ParentView>, entry: &Entry) -> bool {
        let Some(view) = view else {
            return false;
        };
        // The registered parent is this directory, not a namesake.
        if view.parent.ident != entry.parent {
            return false;
        }
        !view.names_allocation(entry)
    }
}

/// An exclusive or shared hold of the registry's custody lock; released when
/// dropped (or when the process ends).
pub(super) struct CustodyGuard {
    _file: std::fs::File,
}

/// Inventories (each listing plus a stat of every name) of one parent that must
/// agree for it to count as a snapshot, and how often they are retried when the
/// directory changes underneath them.
const INVENTORY_ATTEMPTS: usize = 3;

#[cfg(test)]
thread_local! {
    /// Called after the final inventory of a parent view, before it is used.
    pub(super) static AFTER_INVENTORIES: std::cell::Cell<Option<fn()>> =
        const { std::cell::Cell::new(None) };
    /// Called when a view is accepted, before it is used.
    pub(super) static AFTER_VIEW: std::cell::Cell<Option<fn()>> =
        const { std::cell::Cell::new(None) };
}

/// A registered parent directory, authenticated and inventoried: a stable
/// snapshot of every name in it, what each names (identity, birth time, mount)
/// and proof that no mount can mask any of them.
struct ParentView {
    parent: Pinned,
    names: Vec<fsys::Named>,
}

impl ParentView {
    /// `None` (retain everything under this parent) unless the parent can be
    /// authenticated and inventoried as an unmodified, unmasked snapshot.
    fn open(
        path: &Path,
        uid: u32,
        budget: &mut Budget,
        mounts: Option<&[PathBuf]>,
    ) -> Option<Self> {
        // Without a readable mount table a mount point cannot be excluded; with
        // one, a mount below the parent could cover a name in it.
        let mounts = mounts?;
        if mounts.iter().any(|m| m != path && m.starts_with(path)) {
            return None;
        }
        let parent = fsys::open_root(path, uid).ok()?;
        for _ in 0..INVENTORY_ATTEMPTS {
            let before = fsys::dir_stamp(&parent).ok()?;
            let first = fsys::named_inventory(&parent, budget);
            let second = fsys::named_inventory(&parent, budget);
            #[cfg(test)]
            if let Some(hook) = AFTER_INVENTORIES.with(std::cell::Cell::get) {
                hook();
            }
            let (first, second) = match (first, second) {
                (Ok(first), Ok(second)) => (first, second),
                (Err(fsys::InventoryError::Unproven), _)
                | (_, Err(fsys::InventoryError::Unproven)) => {
                    return None;
                }
                // Modified while it was read: look again.
                _ => continue,
            };
            let after = fsys::dir_stamp(&parent).ok()?;
            // A snapshot, as far as it can be established without kernel
            // support: the two inventories agree exactly (name, device, inode,
            // birth time, mount) and hold no repeated directory identity, the
            // directory's own stamps did not move, and the path still names this
            // directory. Residual: stamps have the filesystem's timestamp grain,
            // so several renames inside one tick can leave them equal, and two
            // unsynchronized readings can in principle both miss a name that
            // never left. That needs a same-user process doing exact-timed
            // exchanges, outside the registry's trust boundary, and costs only
            // a dropped registry entry (the tree is kept), never data.
            if first != second || before != after || !fsys::root_unchanged(path, uid, &parent) {
                continue;
            }
            // A name on another mount than its parent is a mount point (a bind
            // mount of the same device included) hiding what is under it.
            let unmasked = first
                .iter()
                .all(|n| fsys::same_known_mount(parent.mount, n.mount));
            if !unmasked || budget.expired() {
                return None;
            }
            #[cfg(test)]
            if let Some(hook) = AFTER_VIEW.with(std::cell::Cell::get) {
                hook();
            }
            return Some(Self {
                parent,
                names: first,
            });
        }
        None
    }

    /// Whether any inventoried name is the allocation: same device and inode,
    /// and not provably a later occupant of a reused inode (a different birth
    /// time). Decided on what the inventory saw, never by reopening a name.
    fn names_allocation(&self, entry: &Entry) -> bool {
        self.names
            .iter()
            .any(|n| n.ident == entry.ident && n.btime.is_none_or(|b| b == entry.btime))
    }
}
