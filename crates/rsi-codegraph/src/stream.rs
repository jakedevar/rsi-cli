//! A streamed index pass (#3491). A corpus whose facts do not fit the
//! extraction cache's retention budget is not held whole: the fact payload that
//! no later step reads (every Markdown file, and the relations and source bytes
//! of every Rust file) is written to an unlinked spill file as soon as a file is
//! extracted, and only the resolver's inputs stay resident, namely each Rust
//! file's nodes, unresolved references and syntax scope. Facts then flow back
//! one file at a time into staging. The result is byte-for-byte the facts the
//! in-memory pass yields; only the peak differs (on this repository's own
//! checkout, 127 MB of source, from 862 MB to the resolver's inputs).

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    CodegraphError, NodeFact, RelationFact, Result, SourceFile, UnresolvedReferenceFact,
    cargo_metadata::{CargoWorkspace, apply_workspace},
    extract::extract_file,
    resolve::{FindingIndex, apply_findings, collect_findings},
    rust_resolve::{self, Outcome},
    scope::RustScope,
    staged::{FileDiagnostic, PerFileFacts},
};

/// The facts of one index pass: the whole corpus in memory, or streamed.
pub enum PassFacts {
    Memory(Vec<PerFileFacts>),
    Streamed(StreamedFacts),
}

impl PassFacts {
    /// Whether the facts live on disk and flow back one file at a time.
    #[must_use]
    pub const fn is_streamed(&self) -> bool {
        matches!(self, Self::Streamed(_))
    }

    /// Each owner's path and the digest of its exact source bytes, in order.
    #[must_use]
    pub fn source_digests(&self) -> Vec<(String, String)> {
        match self {
            Self::Memory(facts) => facts
                .iter()
                .map(|facts| {
                    (
                        facts.file.relative_path.clone(),
                        blake3::hash(&facts.file.bytes).to_hex().to_string(),
                    )
                })
                .collect(),
            Self::Streamed(streamed) => streamed
                .entries
                .iter()
                .map(|entry| (entry.facts.file.relative_path.clone(), entry.digest.clone()))
                .collect(),
        }
    }

    /// Visit every file's final facts in corpus order. A streamed pass loads
    /// one file at a time, so a caller that stages inside `visit` holds a single
    /// file's facts. It may be called again to retry staging.
    ///
    /// # Errors
    /// The first error `visit` returns, or a spill read failure.
    pub fn for_each<E: From<CodegraphError>>(
        &mut self,
        mut visit: impl FnMut(&PerFileFacts) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        match self {
            Self::Memory(facts) => facts.iter().try_for_each(visit),
            Self::Streamed(streamed) => {
                for index in 0..streamed.entries.len() {
                    let facts = streamed.load(index)?;
                    visit(&facts)?;
                }
                Ok(())
            }
        }
    }

    /// Every file's final facts at once, for tests that compare the two paths.
    ///
    /// # Errors
    /// A spill read failure.
    pub fn collect(&mut self) -> Result<Vec<PerFileFacts>> {
        let mut all = Vec::new();
        self.for_each(|facts| {
            all.push(facts.clone());
            Ok::<(), CodegraphError>(())
        })?;
        Ok(all)
    }
}

/// What a file keeps in memory between extraction and staging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Everything stays resident (manifests: small, and Cargo metadata edits them).
    Whole,
    /// Nodes, unresolved references and scope stay for the resolver; relations
    /// and source bytes spill.
    RustSplit,
    /// Only the path and diagnostic stay; every fact and the source bytes spill.
    FullSpill,
}

fn layout_for(path: &str) -> Layout {
    if path.ends_with(".rs") {
        Layout::RustSplit
    } else if path.ends_with(".toml") || path.rsplit('/').next() == Some("Cargo.lock") {
        Layout::Whole
    } else {
        Layout::FullSpill
    }
}

struct Entry {
    digest: String,
    layout: Layout,
    facts: PerFileFacts,
    offset: u64,
    /// Where the relations the Rust resolver proved were spilled.
    resolved_offset: Option<u64>,
    /// Positions of the file's unresolved references the resolver proved.
    dropped: HashSet<usize>,
}

pub struct StreamedFacts {
    entries: Vec<Entry>,
    findings: FindingIndex,
    spill: File,
}

#[derive(Serialize)]
struct SpillOut<'a> {
    nodes: &'a [NodeFact],
    relations: &'a [RelationFact],
    unresolved_references: &'a [UnresolvedReferenceFact],
}

#[derive(Deserialize)]
struct SpillIn {
    nodes: Vec<NodeFact>,
    relations: Vec<RelationFact>,
    unresolved_references: Vec<UnresolvedReferenceFact>,
}

fn spill_error(error: &impl std::fmt::Display) -> CodegraphError {
    CodegraphError::InvalidInput(format!("index spill failed: {error}"))
}

fn empty_facts(path: &str, diagnostic: FileDiagnostic) -> PerFileFacts {
    PerFileFacts {
        file: SourceFile {
            relative_path: path.to_owned(),
            bytes: Vec::new(),
        },
        nodes: Vec::new(),
        relations: Vec::new(),
        unresolved_references: Vec::new(),
        diagnostic,
        scope: RustScope::default(),
    }
}

impl StreamedFacts {
    /// Extract `files` (already validated; `digests` are their source digests),
    /// spilling what the resolver does not read, then resolve.
    ///
    /// # Errors
    /// A spill write failure.
    pub(crate) fn build(
        files: Vec<SourceFile>,
        digests: Vec<String>,
        cargo: Option<&CargoWorkspace>,
        spill_dir: &Path,
    ) -> Result<Self> {
        let mut spill = tempfile::tempfile_in(spill_dir).map_err(|error| spill_error(&error))?;
        let mut slots = files.into_iter().map(Some).collect::<Vec<_>>();
        let mut entries = digests
            .into_iter()
            .map(|digest| Entry {
                digest,
                layout: Layout::Whole,
                facts: empty_facts("", FileDiagnostic::Parsed),
                offset: 0,
                resolved_offset: None,
                dropped: HashSet::new(),
            })
            .collect::<Vec<_>>();
        let mut findings = FindingIndex::new();
        // Markdown first: its bytes leave memory before the Rust facts grow, so
        // the unread source and the resolver's inputs are never both at peak.
        for wanted in [Layout::FullSpill, Layout::Whole, Layout::RustSplit] {
            for (index, slot) in slots.iter_mut().enumerate() {
                if slot
                    .as_ref()
                    .is_none_or(|file| layout_for(&file.relative_path) != wanted)
                {
                    continue;
                }
                let Some(file) = slot.take() else {
                    continue;
                };
                let mut facts = extract_file(file);
                collect_findings(&facts, &mut findings);
                let entry = &mut entries[index];
                entry.layout = wanted;
                match wanted {
                    Layout::Whole => entry.facts = facts,
                    Layout::RustSplit => {
                        entry.offset = write_record(
                            &mut spill,
                            &[],
                            &facts.relations,
                            &facts.unresolved_references,
                            &facts.file.bytes,
                        )?;
                        facts.relations = Vec::new();
                        facts.unresolved_references = Vec::new();
                        facts.file.bytes = Vec::new();
                        facts.nodes.shrink_to_fit();
                        entry.facts = facts;
                    }
                    Layout::FullSpill => {
                        entry.offset = write_record(
                            &mut spill,
                            &facts.nodes,
                            &facts.relations,
                            &facts.unresolved_references,
                            &facts.file.bytes,
                        )?;
                        entry.facts = empty_facts(&facts.file.relative_path, facts.diagnostic);
                    }
                }
            }
        }
        // The resolver reads Rust and manifest files only; hand it exactly
        // those, in corpus order.
        let mut resident = Vec::new();
        let mut positions = Vec::new();
        for (index, entry) in entries.iter_mut().enumerate() {
            if entry.layout != Layout::FullSpill {
                resident.push(std::mem::replace(
                    &mut entry.facts,
                    empty_facts("", FileDiagnostic::Parsed),
                ));
                positions.push(index);
            }
        }
        let offsets = positions
            .iter()
            .map(|index| {
                (entries[*index].layout == Layout::RustSplit).then_some(entries[*index].offset)
            })
            .collect::<Vec<_>>();
        let spill = RefCell::new(spill);
        let failure = RefCell::new(None);
        let mut manifest_outcomes = Vec::new();
        let mut resolved = Vec::new();
        rust_resolve::resolve_each(
            &resident,
            cargo,
            |at| match offsets[at] {
                // A Rust file's references are read from the spill for its turn only.
                Some(offset) => match read_record(&mut spill.borrow_mut(), offset) {
                    Ok((record, _)) => Cow::Owned(record.unresolved_references),
                    Err(error) => {
                        failure.borrow_mut().get_or_insert(error);
                        Cow::Owned(Vec::new())
                    }
                },
                None => Cow::Borrowed(resident[at].unresolved_references.as_slice()),
            },
            |at, outcome| {
                if failure.borrow().is_some() {
                    return;
                }
                if offsets[at].is_none() {
                    // Manifests are resident whole: fold the outcome in below.
                    manifest_outcomes.push((at, outcome));
                    return;
                }
                // A Rust file's proven relations spill at once; its dropped
                // positions are folded in when it is loaded.
                let mut resolved_offset = None;
                if !outcome.relations.is_empty() {
                    match write_record(&mut spill.borrow_mut(), &[], &outcome.relations, &[], &[]) {
                        Ok(offset) => resolved_offset = Some(offset),
                        Err(error) => {
                            failure.borrow_mut().get_or_insert(error);
                            return;
                        }
                    }
                }
                resolved.push((at, resolved_offset, outcome.dropped));
            },
        );
        let spill = spill.into_inner();
        for (at, resolved_offset, dropped) in resolved {
            let entry = &mut entries[positions[at]];
            entry.resolved_offset = resolved_offset;
            entry.dropped = dropped;
        }
        if let Some(error) = failure.into_inner() {
            return Err(error);
        }
        for (at, outcome) in manifest_outcomes {
            fold_outcome(&mut resident[at], &outcome);
        }
        if let Some(metadata) = cargo {
            apply_workspace(&mut resident, metadata);
        }
        for (mut facts, index) in resident.into_iter().zip(positions) {
            facts.scope = RustScope::default();
            entries[index].facts = facts;
        }
        Ok(Self {
            entries,
            findings,
            spill,
        })
    }

    fn load(&mut self, index: usize) -> Result<PerFileFacts> {
        let entry = &self.entries[index];
        let mut facts = entry.facts.clone();
        if entry.layout != Layout::Whole {
            let (record, bytes) = read_record(&mut self.spill, entry.offset)?;
            facts.file.bytes = bytes;
            facts.relations = record.relations;
            facts.unresolved_references = record.unresolved_references;
            if entry.layout == Layout::FullSpill {
                facts.nodes = record.nodes;
            }
        }
        let resolved_offset = entry.resolved_offset;
        let dropped = entry.dropped.clone();
        if let Some(offset) = resolved_offset {
            let (record, _) = read_record(&mut self.spill, offset)?;
            facts.relations.extend(record.relations);
        }
        drop_proven(&mut facts, &dropped);
        apply_findings(&mut facts, &self.findings);
        Ok(facts)
    }
}

fn fold_outcome(facts: &mut PerFileFacts, outcome: &Outcome) {
    if !outcome.relations.is_empty() {
        facts.relations.extend(outcome.relations.iter().cloned());
    }
    drop_proven(facts, &outcome.dropped);
}

fn drop_proven(facts: &mut PerFileFacts, dropped: &HashSet<usize>) {
    if dropped.is_empty() {
        return;
    }
    let mut position = 0usize;
    facts.unresolved_references.retain(|_| {
        let keep = !dropped.contains(&position);
        position += 1;
        keep
    });
}

/// Append `[json len][bytes len][json][bytes]` and return its offset.
fn write_record(
    spill: &mut File,
    nodes: &[NodeFact],
    relations: &[RelationFact],
    unresolved_references: &[UnresolvedReferenceFact],
    bytes: &[u8],
) -> Result<u64> {
    let json = serde_json::to_vec(&SpillOut {
        nodes,
        relations,
        unresolved_references,
    })
    .map_err(|error| spill_error(&error))?;
    let offset = spill
        .seek(SeekFrom::End(0))
        .map_err(|error| spill_error(&error))?;
    let mut record = Vec::with_capacity(16 + json.len() + bytes.len());
    record.extend_from_slice(&(json.len() as u64).to_le_bytes());
    record.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    record.extend_from_slice(&json);
    record.extend_from_slice(bytes);
    spill
        .write_all(&record)
        .map_err(|error| spill_error(&error))?;
    Ok(offset)
}

fn read_record(spill: &mut File, offset: u64) -> Result<(SpillIn, Vec<u8>)> {
    spill
        .seek(SeekFrom::Start(offset))
        .map_err(|error| spill_error(&error))?;
    let mut header = [0u8; 16];
    spill
        .read_exact(&mut header)
        .map_err(|error| spill_error(&error))?;
    let json_len = usize::try_from(u64::from_le_bytes(header[..8].try_into().unwrap_or([0; 8])))
        .map_err(|error| spill_error(&error))?;
    let bytes_len = usize::try_from(u64::from_le_bytes(header[8..].try_into().unwrap_or([0; 8])))
        .map_err(|error| spill_error(&error))?;
    let mut json = vec![0u8; json_len];
    spill
        .read_exact(&mut json)
        .map_err(|error| spill_error(&error))?;
    let mut bytes = vec![0u8; bytes_len];
    spill
        .read_exact(&mut bytes)
        .map_err(|error| spill_error(&error))?;
    let record = serde_json::from_slice(&json).map_err(|error| spill_error(&error))?;
    Ok((record, bytes))
}
