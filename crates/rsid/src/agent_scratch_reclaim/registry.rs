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
//! Registry growth (#1171): a reclaim that deletes an allocation writes a
//! `<nonce>.reclaimed` tombstone once its contents, provenance and directory
//! are gone (and the deletions durable), then removes the entry, manifest and
//! tombstone; the pass-level prune finishes any such removal that was
//! interrupted. Prune drops only tombstoned
//! allocations and never infers absence from a filesystem, so an allocation
//! removed by anything other than this reclaim keeps its entry (a few hundred
//! bytes), and so does one the reclaim stopped on before the tombstone.
//!
//! Trust boundary: the registry lives where only the daemon user can write. A
//! process running as that same user and set on forging provenance can write
//! it too; no file a same-user process can rewrite defends against that. The
//! guarantee is against everything else: copies, moves, stale or replayed
//! records, look-alike names, other tools' directories, and records written by
//! mistake.

use std::io;
use std::path::{Path, PathBuf};

use super::fsys::{self, Budget, Ident, Pinned};

const REGISTRY_DIR_NAME: &str = "scratch-registry";
const ENTRY_MAGIC: &str = "rsi-scratch-alloc v1";
const ENTRY_LIMIT: usize = 16 * 1024;
const MANIFEST_MAGIC: &[u8] = b"rsi-scratch-manifest v1\n";
const MANIFEST_LIMIT: usize = 128 << 20;
const MANIFEST_SUFFIX: &str = ".manifest";
const TOMBSTONE_MAGIC: &str = "rsi-scratch-reclaimed v1";
/// A sibling of the entry (`<nonce>.reclaimed`): an older daemon's registry
/// code skips names that are not a bare nonce or `<nonce>.manifest`.
const TOMBSTONE_SUFFIX: &str = ".reclaimed";

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

#[cfg(test)]
thread_local! {
    /// Test seam: runs right after `put_new` created the entry.
    pub(super) static AFTER_PUT_NEW: std::cell::Cell<Option<fn(&Registry, &Entry)>> =
        const { std::cell::Cell::new(None) };
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

    /// The entry of a live allocation. A reclaimed allocation (tombstoned) has
    /// no entry any more, whether or not the file was removed yet.
    pub(super) fn get(&self, nonce: &str) -> Option<Entry> {
        if !valid_nonce(nonce) || self.tombstone_exists(nonce) {
            return None;
        }
        self.read_entry(nonce)
    }

    fn read_entry(&self, nonce: &str) -> Option<Entry> {
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

    /// Create the entry. A nonce a reclaim already tombstoned is never reused:
    /// a finish still in progress for it could otherwise drop the new entry
    /// (UUID collisions do not happen; this is the safe answer if one is forced).
    pub(super) fn put_new(&self, entry: &Entry) -> io::Result<()> {
        if self.tombstone_exists(&entry.nonce) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "scratch nonce was already reclaimed",
            ));
        }
        fsys::create_file(&self.dir, &entry.nonce, entry.to_text().as_bytes())?;
        #[cfg(test)]
        if let Some(hook) = AFTER_PUT_NEW.with(std::cell::Cell::get) {
            hook(self, entry);
        }
        // A tombstone that appeared between the check and the create (the nonce
        // was reclaimed concurrently): do not leave an entry a finish would
        // drop. Residual: a UUID collision plus a sub-second window.
        if self.tombstone_exists(&entry.nonce) {
            self.remove_entry(&entry.nonce);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "scratch nonce was already reclaimed",
            ));
        }
        Ok(())
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

    /// Completion tombstone for `entry`: this daemon's reclaim has deleted the
    /// allocation's contents and provenance and removed its directory (the
    /// final `rmdir` succeeded, and the deletions were made durable). A
    /// directory kept for any reason (late contents, a failed sync) never gets
    /// one. Written by the reclaim itself, as a fresh file renamed into place
    /// relative to the pinned registry directory, never inferred from what a
    /// filesystem path shows.
    pub(super) fn tombstone_put(&self, entry: &Entry) -> io::Result<()> {
        let text = Tombstone::of(entry, unix_now()).to_text();
        fsys::replace_file(
            &self.dir,
            &Self::tombstone_name(&entry.nonce),
            text.as_bytes(),
        )
    }

    fn tombstone_name(nonce: &str) -> String {
        format!("{nonce}{TOMBSTONE_SUFFIX}")
    }

    /// The valid tombstone of `nonce`: ours, private, parseable, naming `nonce`.
    fn tombstone_get(&self, nonce: &str) -> Option<Tombstone> {
        if !valid_nonce(nonce) {
            return None;
        }
        let (data, stat) = fsys::read_file_at(
            self.dir.raw(),
            &Self::tombstone_name(nonce),
            ENTRY_LIMIT,
            Some(self.uid),
        )
        .ok()
        .flatten()?;
        if fsys::file_mode(&stat) & 0o022 != 0 {
            return None;
        }
        let tombstone = Tombstone::parse(std::str::from_utf8(&data).ok()?)?;
        (tombstone.nonce == nonce).then_some(tombstone)
    }

    /// Whether anything is named `<nonce>.reclaimed`. A name that cannot be
    /// examined counts: a reclaimed allocation is never bound again.
    fn tombstone_exists(&self, nonce: &str) -> bool {
        !matches!(
            fsys::stat_opt(&self.dir, &Self::tombstone_name(nonce)),
            Ok(None)
        )
    }

    /// Remove a reclaimed allocation's registry files: its entry, its manifest,
    /// then (after the removals are durable) its tombstone, so an interrupted
    /// finish is repeated by a later prune. The tombstone is kept unless both
    /// other files are provably gone and the registry directory synced.
    ///
    /// Registry files only; no scratch tree is opened, listed or touched.
    pub(super) fn finish_reclaimed(&self, nonce: &str) -> bool {
        if !valid_nonce(nonce) {
            return false;
        }
        let manifest = Self::manifest_name(nonce);
        fsys::remove_file_quiet(&self.dir, nonce);
        fsys::remove_file_quiet(&self.dir, &manifest);
        let gone = |name: &str| matches!(fsys::stat_opt(&self.dir, name), Ok(None));
        if !(gone(nonce) && gone(&manifest)) {
            return false;
        }
        // The tombstone is the only proof left: it must not become durably
        // gone while the removals above could still be undone by a crash.
        if fsys::sync_dir(&self.dir).is_err() {
            return false;
        }
        fsys::remove_file_quiet(&self.dir, &Self::tombstone_name(nonce));
        true
    }

    /// Drop the registry files of allocations this daemon's reclaim finished
    /// (#1171), at most `max` tombstones per call and within the pass budget,
    /// serialized with every reclaim by the custody lock (a busy lock prunes
    /// nothing this pass).
    ///
    /// The proof is a durable positive record, not an inference: the registry
    /// itself holds a valid tombstone, written by a reclaim after it deleted
    /// the allocation's contents and provenance. Nothing about the filesystem
    /// is examined, so an allocation whose directory was removed by hand or by
    /// another tool keeps its entry (a few hundred bytes), and a reclaim that
    /// stopped before writing the tombstone keeps it too. A tombstone that
    /// does not agree with the entry it names (device, inode, birth time), or
    /// is not ours and private, is left alone with the entry.
    ///
    /// Removes only `<nonce>`, `<nonce>.manifest` and `<nonce>.reclaimed` of a
    /// tombstoned nonce. Never touches a scratch tree.
    pub(super) fn prune(&self, max: usize, budget: &mut Budget) -> usize {
        let Some(_custody) = self.prune_guard() else {
            return 0;
        };
        let Ok(names) = fsys::entry_names(self.dir.raw(), true, budget) else {
            return 0;
        };
        let mut pruned = 0;
        for nonce in tombstone_candidates(&names, max) {
            if budget.expired() {
                break;
            }
            // Re-read under the custody lock, immediately before the drop.
            let Some(tombstone) = self.tombstone_get(&nonce) else {
                continue;
            };
            if !self.agrees_with_entry(&tombstone) {
                continue;
            }
            if self.finish_reclaimed(&nonce) {
                pruned += 1;
            }
        }
        pruned
    }

    /// An entry still present must be readable and be the allocation the
    /// tombstone records; only a proven absence skips the comparison. An entry
    /// whose identity cannot be established is kept, with its tombstone.
    fn agrees_with_entry(&self, tombstone: &Tombstone) -> bool {
        match fsys::stat_opt(&self.dir, &tombstone.nonce) {
            Ok(None) => return true,
            Err(_) => return false,
            Ok(Some(_)) => {}
        }
        self.read_entry(&tombstone.nonce)
            .is_some_and(|e| e.ident == tombstone.ident && e.btime == tombstone.btime)
    }
}

/// The nonces whose tombstones a prune examines: at most `max`, chosen among
/// the tombstone names only, so the (many) live entries cannot crowd them out.
pub(super) fn tombstone_candidates(names: &[String], max: usize) -> Vec<String> {
    names
        .iter()
        .filter_map(|name| name.strip_suffix(TOMBSTONE_SUFFIX))
        .filter(|nonce| valid_nonce(nonce))
        .take(max)
        .map(str::to_string)
        .collect()
}

/// What a reclaim records when it has deleted an allocation: the allocation's
/// nonce and identity, so the record can only ever drop that entry.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tombstone {
    nonce: String,
    ident: Ident,
    btime: i128,
    reclaimed_unix: u64,
}

impl Tombstone {
    fn of(entry: &Entry, reclaimed_unix: u64) -> Self {
        Self {
            nonce: entry.nonce.clone(),
            ident: entry.ident,
            btime: entry.btime,
            reclaimed_unix,
        }
    }

    fn to_text(&self) -> String {
        format!(
            "{TOMBSTONE_MAGIC}\nnonce={}\ndev={}\nino={}\nbtime_ns={}\nreclaimed_unix={}\n",
            self.nonce, self.ident.dev, self.ident.ino, self.btime, self.reclaimed_unix
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != TOMBSTONE_MAGIC {
            return None;
        }
        let (mut nonce, mut dev, mut ino, mut btime, mut at) = (None, None, None, None, None);
        for line in lines {
            let (key, value) = line.split_once('=')?;
            match key {
                "nonce" => nonce = Some(value.to_string()),
                "dev" => dev = value.parse::<u64>().ok(),
                "ino" => ino = value.parse::<u64>().ok(),
                "btime_ns" => btime = value.parse::<i128>().ok(),
                "reclaimed_unix" => at = value.parse::<u64>().ok(),
                _ => {}
            }
        }
        Some(Self {
            nonce: nonce.filter(|n| valid_nonce(n))?,
            ident: Ident {
                dev: dev?,
                ino: ino?,
            },
            btime: btime?,
            reclaimed_unix: at?,
        })
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// An exclusive or shared hold of the registry's custody lock; released when
/// dropped (or when the process ends).
pub(super) struct CustodyGuard {
    _file: std::fs::File,
}
