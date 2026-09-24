//! Exact-byte inventory and reusable raw extraction cache. Resolution is
//! rerun from raw facts on every update so deleted targets cannot leave edges.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::cargo_metadata::{CargoWorkspace, apply_workspace};
use crate::{
    CodegraphError, NodeKind, Result, SourceFile,
    extract::extract_file,
    resolve::resolve_unique,
    staged::{MAX_STAGED_FILE_BYTES, MAX_STAGED_SOURCE_BYTES, PerFileFacts},
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

#[derive(Debug, Default)]
pub struct ExtractionCache {
    raw: BTreeMap<String, PerFileFacts>,
    previous_inventory: Option<SourceInventory>,
    previous_cargo_metadata_digest: Option<String>,
}

impl ExtractionCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update from a complete owner inventory. Unchanged byte versions reuse
    /// raw parser facts; all references are resolved against the new corpus.
    ///
    /// # Errors
    /// Duplicate paths and staged inventory bounds are rejected.
    pub fn update(&mut self, files: Vec<SourceFile>) -> Result<CorpusUpdate> {
        if files.len() > crate::staged::MAX_STAGED_FILES {
            return Err(CodegraphError::LimitExceeded {
                requested: files.len(),
                maximum: crate::staged::MAX_STAGED_FILES,
            });
        }
        let mut next_raw = BTreeMap::new();
        let mut changed = BTreeSet::new();
        let mut source_bytes = 0usize;
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
            if next_raw.contains_key(&path) {
                return Err(CodegraphError::InvalidInput(format!(
                    "duplicate source owner: {path}"
                )));
            }
            let digest = blake3::hash(&file.bytes).to_hex().to_string();
            let previous = self.raw.get(&path);
            let facts = previous
                .filter(|previous| blake3::hash(&previous.file.bytes).to_hex().as_str() == digest)
                .map_or_else(
                    || {
                        changed.insert(path.clone());
                        extract_file(file)
                    },
                    Clone::clone,
                );
            next_raw.insert(path, facts);
        }
        for path in self.raw.keys() {
            if !next_raw.contains_key(path) {
                changed.insert(path.clone());
            }
        }
        let mut facts = next_raw.values().cloned().collect::<Vec<_>>();
        resolve_unique(&mut facts);
        let inventory = SourceInventory::from_facts(&facts);
        let mut invalidated = changed.clone();
        if let Some(previous) = &self.previous_inventory {
            propagate(&previous.dependencies, &mut invalidated);
        }
        propagate(&inventory.dependencies, &mut invalidated);
        self.raw = next_raw;
        self.previous_inventory = Some(inventory.clone());
        Ok(CorpusUpdate {
            facts,
            inventory,
            changed_owners: changed,
            invalidated_owners: invalidated,
        })
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
        let mut update = self.update(files)?;
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
