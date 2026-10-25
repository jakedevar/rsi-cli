//! Exact-byte inventory and reusable raw extraction cache. Resolution is
//! rerun from raw facts on every update so deleted targets cannot leave edges.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use crate::cargo_metadata::{CargoWorkspace, apply_workspace};
use crate::{
    CodegraphError, NodeKind, Result, SourceFile,
    extract::extract_file,
    resolve::resolve_workspace,
    staged::{MAX_STAGED_FILE_BYTES, MAX_STAGED_SOURCE_BYTES, PerFileFacts},
    stream::{PassFacts, StreamedFacts},
    validate_path,
};

/// A complete current set of source owners and their direct dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInventory {
    pub owners: BTreeMap<String, String>,
    /// Owner path to dependency paths, including cross-file resolved sites.
    pub dependencies: BTreeMap<String, BTreeSet<String>>,
    pub digest: String,
    pub cargo_metadata_digest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CorpusUpdate {
    pub facts: Vec<PerFileFacts>,
    pub inventory: SourceInventory,
    pub changed_owners: BTreeSet<String>,
    pub invalidated_owners: BTreeSet<String>,
}

/// Heap one indexing pass holds per byte of Rust source: every file's facts
/// are alive at once while references resolve. Measured on this repository's
/// own crates (#3491: 0.8 MB of `.rs` peaked at 11.5x). Rounded up.
pub const RUST_FACT_HEAP_PER_SOURCE_BYTE: usize = 12;

/// The same for every other indexed file (markdown, TOML): 3.6x measured on
/// 60 MB of markdown, rounded up.
pub const OTHER_FACT_HEAP_PER_SOURCE_BYTE: usize = 4;

/// Estimated heap of the facts of one file of `len` bytes at `path`.
#[must_use]
pub fn estimated_fact_heap_bytes(path: &str, len: usize) -> usize {
    let factor = if path.ends_with(".rs") {
        RUST_FACT_HEAP_PER_SOURCE_BYTE
    } else {
        OTHER_FACT_HEAP_PER_SOURCE_BYTE
    };
    len.saturating_mul(factor)
}

/// Estimated heap of one copy of a corpus's facts that the cache may keep
/// between passes. A larger corpus is re-extracted on the next pass instead, so
/// a pass holds one copy of its facts rather than the cache, a clone and the
/// resolved copy at once (#3491: a hub start with a 193 MB checkout reached
/// 2.8 GB resident). With retention the pass holds two copies, so this is half
/// the heap a retaining pass may use.
pub const DEFAULT_RETAIN_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// Heap a streamed pass holds per byte of Rust source (#3491): the resolver's
/// nodes, syntax scope and working tables, with everything else spilled.
/// Measured on this repository's own checkout: 65 MB of Rust inside 127 MB of source
/// peaked at 166 MB live in all (2.6x its Rust bytes), and 3.7x on a small fixture
/// where one file's extraction transient dominates; rounded up past both.
pub const STREAMED_RUST_HEAP_PER_SOURCE_BYTE: usize = 4;

/// The same for every other file: only its source bytes, which the caller holds
/// until the file is extracted.
pub const STREAMED_OTHER_HEAP_PER_SOURCE_BYTE: usize = 1;

/// Estimated heap of one file in a streamed pass.
#[must_use]
pub fn estimated_streamed_heap_bytes(path: &str, len: usize) -> usize {
    let factor = if path.ends_with(".rs") {
        STREAMED_RUST_HEAP_PER_SOURCE_BYTE
    } else {
        STREAMED_OTHER_HEAP_PER_SOURCE_BYTE
    };
    len.saturating_mul(factor)
}

/// Estimated heap of a whole pass given both per-file sums: a corpus within the
/// default retention budget is held in memory, a larger one is streamed.
#[must_use]
pub const fn estimated_pass_heap_bytes(in_memory: usize, streamed: usize) -> usize {
    if in_memory <= DEFAULT_RETAIN_BUDGET_BYTES {
        in_memory
    } else {
        streamed
    }
}

/// A copy of `facts` that does not clone the source bytes; `bytes` replaces them.
fn copy_with_bytes(facts: &PerFileFacts, bytes: Vec<u8>) -> PerFileFacts {
    PerFileFacts {
        file: SourceFile {
            relative_path: facts.file.relative_path.clone(),
            bytes,
        },
        nodes: facts.nodes.clone(),
        relations: facts.relations.clone(),
        unresolved_references: facts.unresolved_references.clone(),
        diagnostic: facts.diagnostic.clone(),
        scope: facts.scope.clone(),
    }
}

#[derive(Debug)]
pub struct ExtractionCache {
    /// Digest of the exact bytes of every owner in the last complete pass. Always
    /// kept: it decides which owners changed.
    digests: BTreeMap<String, String>,
    /// Unresolved facts per owner, kept only while the estimated heap is within
    /// `retain_budget_bytes`.
    raw: BTreeMap<String, PerFileFacts>,
    retain_budget_bytes: usize,
    previous_inventory: Option<SourceInventory>,
    previous_cargo_metadata_digest: Option<String>,
}

impl Default for ExtractionCache {
    fn default() -> Self {
        Self::with_retain_budget(DEFAULT_RETAIN_BUDGET_BYTES)
    }
}

impl ExtractionCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A cache that keeps raw facts between passes only while one copy of the
    /// corpus's facts (see [`estimated_fact_heap_bytes`]) fits in
    /// `retain_budget_bytes`; zero keeps none.
    #[must_use]
    pub fn with_retain_budget(retain_budget_bytes: usize) -> Self {
        Self {
            digests: BTreeMap::new(),
            raw: BTreeMap::new(),
            retain_budget_bytes,
            previous_inventory: None,
            previous_cargo_metadata_digest: None,
        }
    }

    /// Update from a complete owner inventory. Unchanged byte versions reuse
    /// raw parser facts when they are retained; all references are resolved
    /// against the new corpus.
    ///
    /// # Errors
    /// Duplicate paths and staged inventory bounds are rejected. A rejected
    /// inventory also empties the retained facts, so the next pass extracts again.
    pub fn update(&mut self, files: Vec<SourceFile>) -> Result<CorpusUpdate> {
        self.update_resolved(files, None)
    }

    fn update_resolved(
        &mut self,
        files: Vec<SourceFile>,
        cargo: Option<&CargoWorkspace>,
    ) -> Result<CorpusUpdate> {
        if files.len() > crate::staged::MAX_STAGED_FILES {
            return Err(CodegraphError::LimitExceeded {
                requested: files.len(),
                maximum: crate::staged::MAX_STAGED_FILES,
            });
        }
        let retain = files
            .iter()
            .map(|file| estimated_fact_heap_bytes(&file.relative_path, file.bytes.len()))
            .fold(0usize, usize::saturating_add)
            <= self.retain_budget_bytes;
        // Take the retained facts so a reused entry moves instead of being
        // cloned next to its source.
        let mut previous_raw = std::mem::take(&mut self.raw);
        let previous_digests = std::mem::take(&mut self.digests);
        let mut next_digests = BTreeMap::new();
        let mut next_raw = BTreeMap::new();
        let mut changed = BTreeSet::new();
        let mut source_bytes = 0usize;
        let mut facts = Vec::with_capacity(files.len());
        for file in files {
            let path = file.relative_path.clone();
            validate_path(&path)?;
            if file.bytes.len() > MAX_STAGED_FILE_BYTES {
                return Err(CodegraphError::LimitExceeded {
                    requested: file.bytes.len(),
                    maximum: MAX_STAGED_FILE_BYTES,
                });
            }
            source_bytes = source_bytes.saturating_add(file.bytes.len());
            if source_bytes > MAX_STAGED_SOURCE_BYTES {
                return Err(CodegraphError::LimitExceeded {
                    requested: source_bytes,
                    maximum: MAX_STAGED_SOURCE_BYTES,
                });
            }
            if next_digests.contains_key(&path) {
                return Err(CodegraphError::InvalidInput(format!(
                    "duplicate source owner: {path}"
                )));
            }
            let digest = blake3::hash(&file.bytes).to_hex().to_string();
            let unchanged = previous_digests.get(&path) == Some(&digest);
            if !unchanged {
                changed.insert(path.clone());
            }
            let cached = previous_raw.remove(&path).filter(|_| unchanged);
            let resolved_input = if let Some(cached) = cached {
                let current = copy_with_bytes(&cached, file.bytes);
                if retain {
                    next_raw.insert(path.clone(), cached);
                }
                current
            } else {
                let extracted = extract_file(file);
                if retain {
                    next_raw.insert(path.clone(), copy_with_bytes(&extracted, Vec::new()));
                }
                extracted
            };
            next_digests.insert(path, digest);
            facts.push(resolved_input);
        }
        for path in previous_digests.keys() {
            if !next_digests.contains_key(path) {
                changed.insert(path.clone());
            }
        }
        drop(previous_raw);
        resolve_workspace(&mut facts, cargo);
        let inventory = SourceInventory::from_facts(&facts);
        let mut invalidated = changed.clone();
        if let Some(previous) = &self.previous_inventory {
            propagate(&previous.dependencies, &mut invalidated);
        }
        propagate(&inventory.dependencies, &mut invalidated);
        self.digests = next_digests;
        self.raw = next_raw;
        self.previous_inventory = Some(inventory.clone());
        Ok(CorpusUpdate {
            facts,
            inventory,
            changed_owners: changed,
            invalidated_owners: invalidated,
        })
    }

    /// Whether the last pass kept raw facts for reuse (false when the corpus
    /// exceeded the retention budget).
    #[must_use]
    pub fn retains_raw_facts(&self) -> bool {
        !self.raw.is_empty()
    }

    /// Resolve and inventory Cargo facts together with source facts. A changed
    /// metadata result invalidates workspace manifests even when their bytes
    /// are unchanged (for example after a lockfile update).
    ///
    /// # Errors
    /// Returns the same inventory bounds as [`Self::update`].
    pub fn update_with_cargo(
        &mut self,
        files: Vec<SourceFile>,
        metadata: &CargoWorkspace,
    ) -> Result<CorpusUpdate> {
        let previous = self.previous_inventory.clone();
        let cargo_changed = self.previous_cargo_metadata_digest.as_deref()
            != Some(metadata.metadata_digest.as_str());
        let mut update = self.update_resolved(files, Some(metadata))?;
        apply_workspace(&mut update.facts, metadata);
        update.inventory =
            SourceInventory::from_facts(&update.facts).with_cargo_digest(&metadata.metadata_digest);
        if cargo_changed {
            update.changed_owners.extend(
                metadata
                    .packages
                    .iter()
                    .map(|package| package.manifest_path.clone()),
            );
        }
        let mut invalidated = update.changed_owners.clone();
        if let Some(previous) = previous {
            propagate(&previous.dependencies, &mut invalidated);
        }
        propagate(&update.inventory.dependencies, &mut invalidated);
        update.invalidated_owners = invalidated;
        self.previous_inventory = Some(update.inventory.clone());
        self.previous_cargo_metadata_digest = Some(metadata.metadata_digest.clone());
        Ok(update)
    }
}

/// The result of [`ExtractionCache::update_pass`]: the pass's facts and the
/// owners whose source bytes changed since the previous pass.
pub struct PassUpdate {
    pub facts: PassFacts,
    pub changed_owners: BTreeSet<String>,
}

impl ExtractionCache {
    /// Update from a complete owner inventory for one index pass. A corpus whose
    /// estimated facts fit the retention budget is held in memory and reused
    /// incrementally; a larger one is streamed (see [`crate::stream`]): the
    /// facts spill to an unlinked file in `spill_dir` and flow back one file at
    /// a time, so the pass holds the resolver's inputs rather than the corpus
    /// (#3491). Both return the same final facts. A streamed pass keeps only
    /// digests, so the next pass extracts again, and computes no invalidation
    /// closure (a pass never read it).
    ///
    /// # Errors
    /// The inventory bounds of [`Self::update`], or a spill failure.
    pub fn update_pass(
        &mut self,
        files: Vec<SourceFile>,
        cargo: Option<&CargoWorkspace>,
        spill_dir: &Path,
    ) -> Result<PassUpdate> {
        let estimate = files
            .iter()
            .map(|file| estimated_fact_heap_bytes(&file.relative_path, file.bytes.len()))
            .fold(0usize, usize::saturating_add);
        if estimate <= self.retain_budget_bytes {
            let mut update = match cargo {
                Some(metadata) => self.update_with_cargo(files, metadata)?,
                None => self.update(files)?,
            };
            // The resolver's syntax-local scope is never staged; release it.
            for facts in &mut update.facts {
                facts.scope = crate::scope::RustScope::default();
            }
            return Ok(PassUpdate {
                facts: PassFacts::Memory(update.facts),
                changed_owners: update.changed_owners,
            });
        }
        self.update_streamed(files, cargo, spill_dir)
    }

    fn update_streamed(
        &mut self,
        files: Vec<SourceFile>,
        cargo: Option<&CargoWorkspace>,
        spill_dir: &Path,
    ) -> Result<PassUpdate> {
        if files.len() > crate::staged::MAX_STAGED_FILES {
            return Err(CodegraphError::LimitExceeded {
                requested: files.len(),
                maximum: crate::staged::MAX_STAGED_FILES,
            });
        }
        // Nothing raw is kept: the corpus is over the retention budget.
        self.raw = BTreeMap::new();
        let previous_digests = std::mem::take(&mut self.digests);
        self.previous_inventory = None;
        let mut digests = Vec::with_capacity(files.len());
        let mut next = BTreeMap::new();
        let mut changed = BTreeSet::new();
        let mut source_bytes = 0usize;
        for file in &files {
            validate_path(&file.relative_path)?;
            if file.bytes.len() > MAX_STAGED_FILE_BYTES {
                return Err(CodegraphError::LimitExceeded {
                    requested: file.bytes.len(),
                    maximum: MAX_STAGED_FILE_BYTES,
                });
            }
            source_bytes = source_bytes.saturating_add(file.bytes.len());
            if source_bytes > MAX_STAGED_SOURCE_BYTES {
                return Err(CodegraphError::LimitExceeded {
                    requested: source_bytes,
                    maximum: MAX_STAGED_SOURCE_BYTES,
                });
            }
            let digest = blake3::hash(&file.bytes).to_hex().to_string();
            if next
                .insert(file.relative_path.clone(), digest.clone())
                .is_some()
            {
                return Err(CodegraphError::InvalidInput(format!(
                    "duplicate source owner: {}",
                    file.relative_path
                )));
            }
            if previous_digests.get(&file.relative_path) != Some(&digest) {
                changed.insert(file.relative_path.clone());
            }
            digests.push(digest);
        }
        for path in previous_digests.keys() {
            if !next.contains_key(path) {
                changed.insert(path.clone());
            }
        }
        if let Some(metadata) = cargo {
            if self.previous_cargo_metadata_digest.as_deref()
                != Some(metadata.metadata_digest.as_str())
            {
                changed.extend(
                    metadata
                        .packages
                        .iter()
                        .map(|package| package.manifest_path.clone()),
                );
            }
            self.previous_cargo_metadata_digest = Some(metadata.metadata_digest.clone());
        }
        let streamed = StreamedFacts::build(files, digests, cargo, spill_dir)?;
        self.digests = next;
        Ok(PassUpdate {
            facts: PassFacts::Streamed(streamed),
            changed_owners: changed,
        })
    }
}

impl SourceInventory {
    #[must_use]
    pub fn from_facts(facts: &[PerFileFacts]) -> Self {
        let mut owners = BTreeMap::new();
        let mut node_paths = HashMap::new();
        let mut finding_paths: HashMap<String, BTreeSet<String>> = HashMap::new();
        for file in facts {
            let path = &file.file.relative_path;
            owners.insert(
                path.clone(),
                blake3::hash(&file.file.bytes).to_hex().to_string(),
            );
            for node in &file.nodes {
                node_paths.insert(node.key.clone(), path.clone());
                if node.kind == NodeKind::ResearchFinding
                    && let Some(id) = node.name.get(..5)
                {
                    finding_paths
                        .entry(id.to_owned())
                        .or_default()
                        .insert(path.clone());
                }
            }
        }
        let mut dependencies = BTreeMap::new();
        for file in facts {
            let path = &file.file.relative_path;
            let mut paths = BTreeSet::new();
            for relation in &file.relations {
                if let Some(target) = node_paths.get(&relation.target)
                    && target != path
                {
                    paths.insert(target.clone());
                }
            }
            for reference in &file.unresolved_references {
                if reference.kind == crate::UnresolvedReferenceKind::Finding
                    && let Some(candidates) = finding_paths.get(&reference.raw_target)
                {
                    paths.extend(candidates.iter().filter(|target| *target != path).cloned());
                }
            }
            dependencies.insert(path.clone(), paths);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rsi-codegraph-source-inventory-v1");
        for (path, digest) in &owners {
            hash_field(&mut hasher, path.as_bytes());
            hash_field(&mut hasher, digest.as_bytes());
        }
        Self {
            owners,
            dependencies,
            digest: hasher.finalize().to_hex().to_string(),
            cargo_metadata_digest: None,
        }
    }

    fn with_cargo_digest(mut self, cargo_digest: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rsi-codegraph-cargo-inventory-v1");
        hash_field(&mut hasher, self.digest.as_bytes());
        hash_field(&mut hasher, cargo_digest.as_bytes());
        self.digest = hasher.finalize().to_hex().to_string();
        self.cargo_metadata_digest = Some(cargo_digest.into());
        self
    }
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn propagate(
    dependencies: &BTreeMap<String, BTreeSet<String>>,
    invalidated: &mut BTreeSet<String>,
) {
    loop {
        let before = invalidated.len();
        for (owner, targets) in dependencies {
            if targets.iter().any(|target| invalidated.contains(target)) {
                invalidated.insert(owner.clone());
            }
        }
        if invalidated.len() == before {
            break;
        }
    }
}
