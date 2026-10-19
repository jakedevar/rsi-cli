//! Content-shared physical storage (schema v9).
//!
//! Until v8 every ready generation of every workspace owned a full copy of its
//! facts, keyed `(workspace_id, generation, ...)`: a project's database grew by
//! the whole graph per publish and per sandbox (#2245). From v9 a generation is
//! a **manifest** of immutable, content-addressed **blocks**:
//!
//! * a *structure* block (`S`) holds one source file's nodes, their evidence,
//!   its structural relations (`Contains`, `Declares`, `HasMethod`, `Defines`),
//!   and carries the file's path, source digest, identity and diagnostic;
//! * a *resolution* block (`R`) holds the same file's resolver output: resolved
//!   relations with their evidence, and the unresolved reference sites that
//!   remain.
//!
//! A block is keyed by the BLAKE3 digest of its exact rows, so two generations,
//! or two workspaces, that extract the same file to the same facts store those
//! rows once. The split keeps a file's structure shared when only its
//! cross-file resolution changes. Reference counts live on the block; pruning a
//! generation drops its manifest rows and deletes only blocks nothing else
//! references. The logical `cg_files`, `cg_nodes`, `cg_relations`,
//! `cg_evidence`, `cg_unresolved_references`, `cg_file_diagnostics` and
//! `cg_fts_nodes` names remain as read-only views with their v8 columns, so
//! diagnostic SQL keeps working; the crate's own reads address the block tables
//! directly with a manifest membership probe.

use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{
    CodegraphError, EvidenceFact, FactKey, NodeFact, RELATION_NS, RelationFact, RelationKind,
    Result, SourceSpan, UnresolvedReferenceFact, evidence_digest, evidence_order, node_id,
    relation_id, unresolved_digest,
};

const BLOCK_DOMAIN: &[u8] = b"rsi-codegraph-block-v1";

/// Per-row accounting overhead, matching the v5 detailed-byte estimate.
const ROW_OVERHEAD_BYTES: i64 = 32;
/// Accounting charge for one manifest row.
const MANIFEST_ROW_BYTES: i64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Structure,
    Resolution,
}

impl BlockKind {
    const fn code(self) -> &'static str {
        match self {
            Self::Structure => "S",
            Self::Resolution => "R",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NodeRow {
    pub id: String,
    pub key: String,
    pub kind: String,
    pub name: String,
    pub span: SourceSpan,
    pub provenance: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RelationRow {
    pub id: String,
    pub key: String,
    pub kind: String,
    pub source: String,
    pub target: String,
    pub provenance: String,
}

#[derive(Debug, Clone)]
pub(crate) struct EvidenceRow {
    pub fact_id: String,
    pub fact_type: String,
    pub ordinal: usize,
    pub label: String,
    pub span: SourceSpan,
    pub source_digest: String,
    pub evidence_digest: String,
}

#[derive(Debug, Clone)]
pub(crate) struct UnresolvedRow {
    pub id: String,
    pub site_key: String,
    pub kind: String,
    pub owner: String,
    pub raw_target: String,
    pub span: SourceSpan,
    pub source_digest: String,
    pub reference_digest: String,
    pub provenance: String,
}

/// The exact rows of one block before it is addressed by content.
#[derive(Debug, Clone)]
pub(crate) struct BlockContent {
    pub kind: BlockKind,
    pub path: String,
    pub source_digest: String,
    pub file_id: Option<String>,
    pub diagnostic: Option<String>,
    pub nodes: Vec<NodeRow>,
    pub relations: Vec<RelationRow>,
    pub evidence: Vec<EvidenceRow>,
    pub unresolved: Vec<UnresolvedRow>,
}

/// A byte length as the signed accounting unit; saturates instead of wrapping.
fn size(length: usize) -> i64 {
    i64::try_from(length).unwrap_or(i64::MAX)
}

fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn number(hasher: &mut blake3::Hasher, value: usize) {
    hasher.update(&(value as u64).to_be_bytes());
}

fn span_columns(hasher: &mut blake3::Hasher, span: &SourceSpan) {
    for value in [
        span.start_byte,
        span.end_byte,
        span.start_line,
        span.start_column,
        span.end_line,
        span.end_column,
    ] {
        number(hasher, value);
    }
}

impl BlockContent {
    pub(crate) fn new(
        kind: BlockKind,
        path: &str,
        source_digest: &str,
        file_id: Option<String>,
        diagnostic: Option<String>,
    ) -> Self {
        Self {
            kind,
            path: path.to_owned(),
            source_digest: source_digest.to_owned(),
            file_id,
            diagnostic,
            nodes: Vec::new(),
            relations: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.relations.is_empty()
            && self.evidence.is_empty()
            && self.unresolved.is_empty()
    }

    /// Canonically order the rows, then return the content digest and the
    /// accounting size. Equal digests mean byte-identical stored rows.
    fn seal(&mut self) -> (String, i64) {
        self.nodes.sort_by(|a, b| a.id.cmp(&b.id));
        self.relations.sort_by(|a, b| a.id.cmp(&b.id));
        self.evidence
            .sort_by(|a, b| (a.fact_id.as_str(), a.ordinal).cmp(&(b.fact_id.as_str(), b.ordinal)));
        self.unresolved.sort_by(|a, b| a.id.cmp(&b.id));
        let mut hasher = blake3::Hasher::new();
        let mut bytes = ROW_OVERHEAD_BYTES;
        field(&mut hasher, BLOCK_DOMAIN);
        field(&mut hasher, self.kind.code().as_bytes());
        field(&mut hasher, self.path.as_bytes());
        field(&mut hasher, self.source_digest.as_bytes());
        match &self.diagnostic {
            Some(diagnostic) => {
                number(&mut hasher, 1);
                field(&mut hasher, diagnostic.as_bytes());
                bytes += size(diagnostic.len());
            }
            None => number(&mut hasher, 0),
        }
        bytes += size(self.path.len() + self.source_digest.len());
        number(&mut hasher, self.nodes.len());
        for row in &self.nodes {
            for text in [&row.id, &row.key, &row.kind, &row.name, &row.provenance] {
                field(&mut hasher, text.as_bytes());
                bytes += size(text.len());
            }
            span_columns(&mut hasher, &row.span);
            bytes += ROW_OVERHEAD_BYTES;
        }
        number(&mut hasher, self.relations.len());
        for row in &self.relations {
            for text in [
                &row.id,
                &row.key,
                &row.kind,
                &row.source,
                &row.target,
                &row.provenance,
            ] {
                field(&mut hasher, text.as_bytes());
                bytes += size(text.len());
            }
            bytes += ROW_OVERHEAD_BYTES;
        }
        number(&mut hasher, self.evidence.len());
        for row in &self.evidence {
            for text in [
                &row.fact_id,
                &row.fact_type,
                &row.label,
                &row.span.path,
                &row.source_digest,
                &row.evidence_digest,
            ] {
                field(&mut hasher, text.as_bytes());
                bytes += size(text.len());
            }
            number(&mut hasher, row.ordinal);
            span_columns(&mut hasher, &row.span);
            bytes += ROW_OVERHEAD_BYTES;
        }
        number(&mut hasher, self.unresolved.len());
        for row in &self.unresolved {
            for text in [
                &row.id,
                &row.site_key,
                &row.kind,
                &row.owner,
                &row.raw_target,
                &row.source_digest,
                &row.reference_digest,
                &row.provenance,
            ] {
                field(&mut hasher, text.as_bytes());
                bytes += size(text.len());
            }
            span_columns(&mut hasher, &row.span);
            bytes += ROW_OVERHEAD_BYTES;
        }
        (hasher.finalize().to_hex().to_string(), bytes)
    }
}

/// Relations the extractors emit from a file's own syntax. Everything else is
/// resolver or Cargo output that can change when another file changes, so it
/// goes in the resolution block. The split only decides what is shared when a
/// file's resolution changes; reads see the union either way.
pub(crate) fn is_structural_kind(kind: &str) -> bool {
    matches!(kind, "Contains" | "Declares" | "HasMethod" | "Defines")
}

/// Whether `kind` is structural, for a typed relation.
fn structural(kind: RelationKind) -> bool {
    is_structural_kind(kind.as_str())
}

/// Workspace-independent identity of one unresolved site, so the same site
/// stored by two workspaces is the same row.
pub(crate) fn unresolved_id(site_key: &str) -> Uuid {
    Uuid::new_v5(&RELATION_NS, format!("unresolved\0{site_key}").as_bytes())
}

fn evidence_row(
    fact: Uuid,
    fact_type: &str,
    ordinal: usize,
    item: &EvidenceFact,
    digest_of: &dyn Fn(&str) -> Result<String>,
) -> Result<EvidenceRow> {
    let source_digest = digest_of(&item.span.path)?;
    Ok(EvidenceRow {
        fact_id: fact.to_string(),
        fact_type: fact_type.to_owned(),
        ordinal,
        label: item.label.clone(),
        span: item.span.clone(),
        evidence_digest: evidence_digest(&source_digest, item),
        source_digest,
    })
}

/// One source file's facts, ready to be addressed as blocks.
pub(crate) struct FileFacts<'a> {
    pub path: &'a str,
    pub source_digest: &'a str,
    pub file_id: Uuid,
    pub diagnostic: Option<String>,
    pub nodes: &'a [NodeFact],
    pub relations: &'a [RelationFact],
    pub unresolved: &'a [UnresolvedReferenceFact],
}

/// Convert one file's facts into its structure block and, when it has any
/// resolver output, its resolution block. `resolve` maps a fact key to its node
/// identity anywhere in the generation; `digest_of` maps an evidence path to the
/// source digest of that file.
pub(crate) fn build_file_blocks(
    repository: Uuid,
    file: &FileFacts<'_>,
    resolve: &dyn Fn(&FactKey) -> Result<Uuid>,
    digest_of: &dyn Fn(&str) -> Result<String>,
) -> Result<(BlockContent, Option<BlockContent>)> {
    let mut structure = BlockContent::new(
        BlockKind::Structure,
        file.path,
        file.source_digest,
        Some(file.file_id.to_string()),
        file.diagnostic.clone(),
    );
    let mut resolution = BlockContent::new(
        BlockKind::Resolution,
        file.path,
        file.source_digest,
        None,
        None,
    );
    for node in file.nodes {
        let id = node_id(repository, node);
        structure.nodes.push(NodeRow {
            id: id.to_string(),
            key: node.key.0.clone(),
            kind: node.kind.as_str().to_owned(),
            name: node.name.clone(),
            span: node.span.clone(),
            provenance: node.provenance.as_str().to_owned(),
        });
        let mut evidence = node.evidence.iter().collect::<Vec<_>>();
        evidence.sort_by(|a, b| evidence_order(a, b));
        for (ordinal, item) in evidence.into_iter().enumerate() {
            structure
                .evidence
                .push(evidence_row(id, "node", ordinal, item, digest_of)?);
        }
    }
    for relation in file.relations {
        let source = resolve(&relation.source)?;
        let target = resolve(&relation.target)?;
        let id = relation_id(
            repository,
            relation.kind,
            source,
            target,
            &relation.owner_file,
            &relation.site_anchor,
        );
        let block = if structural(relation.kind) {
            &mut structure
        } else {
            &mut resolution
        };
        block.relations.push(RelationRow {
            id: id.to_string(),
            key: relation.key.0.clone(),
            kind: relation.kind.as_str().to_owned(),
            source: source.to_string(),
            target: target.to_string(),
            provenance: relation.provenance.as_str().to_owned(),
        });
        let mut evidence = relation.evidence.iter().collect::<Vec<_>>();
        evidence.sort_by(|a, b| evidence_order(a, b));
        for (ordinal, item) in evidence.into_iter().enumerate() {
            block
                .evidence
                .push(evidence_row(id, "relation", ordinal, item, digest_of)?);
        }
    }
    for reference in file.unresolved {
        let owner = resolve(&reference.owner)?;
        let source_digest = digest_of(&reference.span.path)?;
        resolution.unresolved.push(UnresolvedRow {
            id: unresolved_id(&reference.key.0).to_string(),
            site_key: reference.key.0.clone(),
            kind: reference.kind.as_str().to_owned(),
            owner: owner.to_string(),
            raw_target: reference.raw_target.clone(),
            span: reference.span.clone(),
            reference_digest: unresolved_digest(&source_digest, reference),
            source_digest,
            provenance: reference.provenance.as_str().to_owned(),
        });
    }
    Ok((structure, (!resolution.is_empty()).then_some(resolution)))
}

/// Return the stored block with this content, inserting it when absent.
pub(crate) fn ensure_block(tx: &Transaction<'_>, mut content: BlockContent) -> Result<i64> {
    let (digest, bytes) = content.seal();
    let existing: Option<i64> = tx
        .prepare_cached("SELECT block_id FROM cg_blocks WHERE digest=?1")?
        .query_row([&digest], |row| row.get(0))
        .optional()?;
    if let Some(block_id) = existing {
        return Ok(block_id);
    }
    tx.prepare_cached(
        "INSERT INTO cg_blocks(kind,digest,path,source_digest,file_id,diagnostic,bytes,refs)
         VALUES (?1,?2,?3,?4,?5,?6,?7,0)",
    )?
    .execute(params![
        content.kind.code(),
        digest,
        content.path,
        content.source_digest,
        content.file_id,
        content.diagnostic,
        bytes
    ])?;
    let block_id = tx.last_insert_rowid();
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO cg_block_nodes(block_id,node_id,node_key,kind,name,start_byte,end_byte,
             start_line,start_column,end_line,end_column,provenance)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        )?;
        let mut index =
            tx.prepare_cached("INSERT INTO cg_fts_blocks(rowid,name,path) VALUES (?1,?2,?3)")?;
        for row in &content.nodes {
            let span = &row.span;
            insert.execute(params![
                block_id,
                row.id,
                row.key,
                row.kind,
                row.name,
                span.start_byte,
                span.end_byte,
                span.start_line,
                span.start_column,
                span.end_line,
                span.end_column,
                row.provenance
            ])?;
            index.execute(params![tx.last_insert_rowid(), row.name, content.path])?;
        }
    }
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO cg_block_relations(block_id,relation_id,relation_key,kind,source_id,target_id,provenance)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
        )?;
        for row in &content.relations {
            insert.execute(params![
                block_id,
                row.id,
                row.key,
                row.kind,
                row.source,
                row.target,
                row.provenance
            ])?;
        }
    }
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO cg_block_evidence(block_id,fact_id,fact_type,ordinal,label,path,start_byte,
             end_byte,start_line,start_column,end_line,end_column,source_digest,evidence_digest)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        )?;
        for row in &content.evidence {
            let span = &row.span;
            insert.execute(params![
                block_id,
                row.fact_id,
                row.fact_type,
                row.ordinal,
                row.label,
                span.path,
                span.start_byte,
                span.end_byte,
                span.start_line,
                span.start_column,
                span.end_line,
                span.end_column,
                row.source_digest,
                row.evidence_digest
            ])?;
        }
    }
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO cg_block_unresolved(block_id,unresolved_id,site_key,kind,owner_id,raw_target,
             start_byte,end_byte,start_line,start_column,end_line,end_column,source_digest,
             reference_digest,provenance)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        )?;
        for row in &content.unresolved {
            let span = &row.span;
            insert.execute(params![
                block_id,
                row.id,
                row.site_key,
                row.kind,
                row.owner,
                row.raw_target,
                span.start_byte,
                span.end_byte,
                span.start_line,
                span.start_column,
                span.end_line,
                span.end_column,
                row.source_digest,
                row.reference_digest,
                row.provenance
            ])?;
        }
    }
    Ok(block_id)
}

/// Add one block to a generation's manifest and count the reference.
pub(crate) fn attach_block(
    tx: &Transaction<'_>,
    workspace: &str,
    generation: i64,
    block_id: i64,
) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO cg_manifest(workspace_id,generation,block_id) VALUES (?1,?2,?3)",
    )?
    .execute(params![workspace, generation, block_id])?;
    tx.prepare_cached("UPDATE cg_blocks SET refs=refs+1 WHERE block_id=?1")?
        .execute([block_id])?;
    Ok(())
}

/// The blocks one file occupies in a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileBlocks {
    pub structure: i64,
    pub resolution: Option<i64>,
}

/// Store one file's blocks and add them to the generation's manifest.
pub(crate) fn store_file_blocks(
    tx: &Transaction<'_>,
    workspace: &str,
    generation: i64,
    blocks: (BlockContent, Option<BlockContent>),
) -> Result<FileBlocks> {
    let (structure, resolution) = blocks;
    let structure = ensure_block(tx, structure)?;
    attach_block(tx, workspace, generation, structure)?;
    let resolution = match resolution {
        Some(resolution) => {
            let block_id = ensure_block(tx, resolution)?;
            attach_block(tx, workspace, generation, block_id)?;
            Some(block_id)
        }
        None => None,
    };
    Ok(FileBlocks {
        structure,
        resolution,
    })
}

/// One staged file that points at blocks already stored (#2351). The staged
/// payload of such a file is this reference, not its facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PayloadRef {
    pub path: String,
    pub source_digest: String,
    pub payload_digest: String,
    pub blocks: FileBlocks,
}

/// Remember which blocks a file's canonical payload was stored as, so the next
/// run that stages the same payload can stage a reference instead.
pub(crate) fn record_payload_ref(tx: &Transaction<'_>, reference: &PayloadRef) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO cg_payload_refs(path,source_digest,payload_digest,structure_block,resolution_block)
         VALUES (?1,?2,?3,?4,?5)
         ON CONFLICT(path,source_digest,payload_digest) DO UPDATE SET
           structure_block=excluded.structure_block,resolution_block=excluded.resolution_block
         WHERE cg_payload_refs.structure_block IS NOT excluded.structure_block
            OR cg_payload_refs.resolution_block IS NOT excluded.resolution_block",
    )?
    .execute(params![
        reference.path,
        reference.source_digest,
        reference.payload_digest,
        reference.blocks.structure,
        reference.blocks.resolution
    ])?;
    Ok(())
}

/// Whether both blocks of `reference` are stored with the identity the
/// reference recorded. A reclaimed block (or a reused row id) fails this.
pub(crate) fn payload_ref_blocks_exist(
    tx: &Transaction<'_>,
    reference: &PayloadRef,
) -> Result<bool> {
    let exists = |block_id: i64, kind: &str| -> Result<bool> {
        Ok(tx
            .prepare_cached(
                "SELECT 1 FROM cg_blocks WHERE block_id=?1 AND kind=?2 AND path=?3 AND source_digest=?4",
            )?
            .query_row(
                params![block_id, kind, reference.path, reference.source_digest],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    };
    Ok(exists(reference.blocks.structure, "S")?
        && match reference.blocks.resolution {
            Some(block_id) => exists(block_id, "R")?,
            None => true,
        })
}

/// The stored blocks that hold exactly this payload of this source version.
pub(crate) fn find_payload_ref(
    tx: &Transaction<'_>,
    path: &str,
    source_digest: &str,
    payload_digest: &str,
) -> Result<Option<PayloadRef>> {
    let found: Option<(i64, Option<i64>)> = tx
        .prepare_cached(
            "SELECT structure_block,resolution_block FROM cg_payload_refs
             WHERE path=?1 AND source_digest=?2 AND payload_digest=?3",
        )?
        .query_row(params![path, source_digest, payload_digest], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    let Some((structure, resolution)) = found else {
        return Ok(None);
    };
    let reference = PayloadRef {
        path: path.to_owned(),
        source_digest: source_digest.to_owned(),
        payload_digest: payload_digest.to_owned(),
        blocks: FileBlocks {
            structure,
            resolution,
        },
    };
    Ok(payload_ref_blocks_exist(tx, &reference)?.then_some(reference))
}

/// Forget a reference that no longer names live blocks.
pub(crate) fn drop_payload_ref(tx: &Transaction<'_>, reference: &PayloadRef) -> Result<()> {
    tx.prepare_cached(
        "DELETE FROM cg_payload_refs WHERE path=?1 AND source_digest=?2 AND payload_digest=?3",
    )?
    .execute(params![
        reference.path,
        reference.source_digest,
        reference.payload_digest
    ])?;
    Ok(())
}

/// The blocks of a reference as one list.
fn reference_blocks(reference: &PayloadRef) -> impl Iterator<Item = i64> {
    std::iter::once(reference.blocks.structure).chain(reference.blocks.resolution)
}

/// Every node of the referenced structure block as `(node_key, node_id)`.
pub(crate) fn referenced_nodes(
    tx: &Transaction<'_>,
    reference: &PayloadRef,
) -> Result<Vec<(String, Uuid)>> {
    let mut statement =
        tx.prepare_cached("SELECT node_key,node_id FROM cg_block_nodes WHERE block_id=?1")?;
    let rows = statement.query_map([reference.blocks.structure], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut nodes = Vec::new();
    for row in rows {
        let (key, id) = row?;
        let id = Uuid::parse_str(&id).map_err(|error| legacy_error(error.to_string()))?;
        nodes.push((key, id));
    }
    Ok(nodes)
}

/// The diagnostic JSON the structure block carries.
pub(crate) fn referenced_diagnostic(
    tx: &Transaction<'_>,
    reference: &PayloadRef,
) -> Result<Option<String>> {
    Ok(tx
        .prepare_cached("SELECT diagnostic FROM cg_blocks WHERE block_id=?1")?
        .query_row([reference.blocks.structure], |row| row.get(0))?)
}

/// Whether every node a referenced block's relations and unresolved sites name
/// still exists in the generation being published. A relation into a node that
/// is gone cannot be published, so the reference must not be used.
pub(crate) fn referenced_endpoints_resolve(
    tx: &Transaction<'_>,
    reference: &PayloadRef,
    nodes: &HashSet<Uuid>,
) -> Result<bool> {
    let known = |id: String| -> Result<bool> {
        Ok(Uuid::parse_str(&id).is_ok_and(|id| nodes.contains(&id)))
    };
    for block_id in reference_blocks(reference) {
        let mut relations = tx.prepare_cached(
            "SELECT source_id,target_id FROM cg_block_relations WHERE block_id=?1",
        )?;
        let mut rows = relations.query([block_id])?;
        while let Some(row) = rows.next()? {
            if !known(row.get(0)?)? || !known(row.get(1)?)? {
                return Ok(false);
            }
        }
        let mut sites =
            tx.prepare_cached("SELECT owner_id FROM cg_block_unresolved WHERE block_id=?1")?;
        let mut rows = sites.query([block_id])?;
        while let Some(row) = rows.next()? {
            if !known(row.get(0)?)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Offer every relation key and unresolved-site key of a referenced file to the
/// duplicate-key sets a publish keeps. Returns the first repeated key.
pub(crate) fn referenced_keys(
    tx: &Transaction<'_>,
    reference: &PayloadRef,
    relation_keys: &mut KeySet,
    site_keys: &mut KeySet,
) -> Result<Option<String>> {
    for block_id in reference_blocks(reference) {
        let mut relations =
            tx.prepare_cached("SELECT relation_key FROM cg_block_relations WHERE block_id=?1")?;
        let mut rows = relations.query([block_id])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            if !relation_keys.insert(&key) {
                return Ok(Some(format!("relation_key {key}")));
            }
        }
        let mut sites =
            tx.prepare_cached("SELECT site_key FROM cg_block_unresolved WHERE block_id=?1")?;
        let mut rows = sites.query([block_id])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            if !site_keys.insert(&key) {
                return Ok(Some(format!("site_key {key}")));
            }
        }
    }
    Ok(None)
}

/// Add a referenced file's blocks to a generation's manifest.
pub(crate) fn attach_referenced(
    tx: &Transaction<'_>,
    workspace: &str,
    generation: i64,
    reference: &PayloadRef,
) -> Result<()> {
    for block_id in reference_blocks(reference) {
        attach_block(tx, workspace, generation, block_id)?;
    }
    Ok(())
}

/// Remove a generation's manifest and delete the blocks nothing else uses.
pub(crate) fn detach_generation(
    tx: &Transaction<'_>,
    workspace: &str,
    generation: i64,
) -> Result<()> {
    tx.execute(
        "UPDATE cg_blocks SET refs=refs-1 WHERE block_id IN
         (SELECT block_id FROM cg_manifest WHERE workspace_id=?1 AND generation=?2)",
        params![workspace, generation],
    )?;
    tx.execute(
        "DELETE FROM cg_manifest WHERE workspace_id=?1 AND generation=?2",
        params![workspace, generation],
    )?;
    delete_unreferenced_blocks(tx)
}

fn delete_unreferenced_blocks(tx: &Transaction<'_>) -> Result<()> {
    let orphans = {
        let mut statement = tx.prepare("SELECT block_id FROM cg_blocks WHERE refs <= 0")?;
        statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for block_id in orphans {
        let indexed = {
            let mut statement =
                tx.prepare_cached("SELECT node_ix FROM cg_block_nodes WHERE block_id=?1")?;
            statement
                .query_map([block_id], |row| row.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for node_ix in indexed {
            tx.prepare_cached("DELETE FROM cg_fts_blocks WHERE rowid=?1")?
                .execute([node_ix])?;
        }
        for table in [
            "cg_block_unresolved",
            "cg_block_evidence",
            "cg_block_relations",
            "cg_block_nodes",
        ] {
            tx.prepare_cached(&format!("DELETE FROM {table} WHERE block_id=?1"))?
                .execute([block_id])?;
        }
        tx.prepare_cached(
            "DELETE FROM cg_payload_refs WHERE structure_block=?1 OR resolution_block=?1",
        )?
        .execute([block_id])?;
        tx.prepare_cached("DELETE FROM cg_blocks WHERE block_id=?1")?
            .execute([block_id])?;
    }
    Ok(())
}

/// Bytes a generation adds to the database: its manifest rows plus the blocks
/// no other generation shares. A generation whose blocks all exist elsewhere
/// is charged its manifest only.
pub(crate) fn marginal_generation_bytes(
    tx: &Transaction<'_>,
    workspace: &str,
    generation: i64,
) -> Result<i64> {
    let total: i64 = tx.query_row(
        "SELECT COALESCE(SUM(CASE WHEN b.refs <= 1 THEN b.bytes ELSE 0 END),0) + COUNT(*) * ?3
         FROM cg_manifest m JOIN cg_blocks b ON b.block_id=m.block_id
         WHERE m.workspace_id=?1 AND m.generation=?2",
        params![workspace, generation, MANIFEST_ROW_BYTES],
        |row| row.get(0),
    )?;
    Ok(total)
}

/// SQL predicate: the block `alias.block_id` belongs to the generation bound as
/// `?1` (workspace) and `?2` (generation). A correlated probe on the manifest's
/// primary key, so a lookup by node, name or path never scans the manifest.
pub(crate) fn scope_sql(alias: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM cg_manifest m WHERE m.workspace_id=?1 AND m.generation=?2 AND m.block_id={alias}.block_id)"
    )
}

/// The v9 physical tables and indexes.
const SCHEMA_V9_TABLES: &str = "
CREATE TABLE cg_blocks (
    block_id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('S','R')),
    digest TEXT NOT NULL UNIQUE,
    path TEXT NOT NULL,
    source_digest TEXT NOT NULL,
    file_id TEXT,
    diagnostic TEXT,
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    refs INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX cg_blocks_path ON cg_blocks(path, kind);
CREATE INDEX cg_blocks_unreferenced ON cg_blocks(block_id) WHERE refs <= 0;
CREATE TABLE cg_manifest (
    workspace_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    block_id INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, generation, block_id)
) WITHOUT ROWID;
CREATE TABLE cg_block_nodes (
    node_ix INTEGER PRIMARY KEY,
    block_id INTEGER NOT NULL,
    node_id TEXT NOT NULL,
    node_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    start_line INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    end_column INTEGER NOT NULL,
    provenance TEXT NOT NULL CHECK (provenance IN ('Extracted','Inferred','Ambiguous')),
    UNIQUE (block_id, node_id)
);
CREATE INDEX cg_block_nodes_id ON cg_block_nodes(node_id, block_id);
CREATE INDEX cg_block_nodes_name ON cg_block_nodes(name, block_id);
CREATE INDEX cg_block_nodes_key ON cg_block_nodes(node_key, block_id);
CREATE TABLE cg_block_relations (
    block_id INTEGER NOT NULL,
    relation_id TEXT NOT NULL,
    relation_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    provenance TEXT NOT NULL CHECK (provenance IN ('Extracted','Inferred','Ambiguous')),
    PRIMARY KEY (block_id, relation_id)
) WITHOUT ROWID;
CREATE INDEX cg_block_relations_id ON cg_block_relations(relation_id, block_id);
CREATE INDEX cg_block_relations_source ON cg_block_relations(source_id, block_id);
CREATE INDEX cg_block_relations_target ON cg_block_relations(target_id, kind, block_id);
CREATE TABLE cg_block_evidence (
    block_id INTEGER NOT NULL,
    fact_id TEXT NOT NULL,
    fact_type TEXT NOT NULL CHECK (fact_type IN ('node','relation')),
    ordinal INTEGER NOT NULL,
    label TEXT NOT NULL,
    path TEXT NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    start_line INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    end_column INTEGER NOT NULL,
    source_digest TEXT NOT NULL,
    evidence_digest TEXT NOT NULL,
    PRIMARY KEY (block_id, fact_id, ordinal)
) WITHOUT ROWID;
CREATE INDEX cg_block_evidence_fact ON cg_block_evidence(fact_id, fact_type, block_id);
CREATE TABLE cg_block_unresolved (
    block_id INTEGER NOT NULL,
    unresolved_id TEXT NOT NULL,
    site_key TEXT NOT NULL,
    kind TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    raw_target TEXT NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    start_line INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    end_column INTEGER NOT NULL,
    source_digest TEXT NOT NULL,
    reference_digest TEXT NOT NULL,
    provenance TEXT NOT NULL CHECK (provenance IN ('Extracted','Inferred','Ambiguous')),
    PRIMARY KEY (block_id, unresolved_id)
) WITHOUT ROWID;
CREATE INDEX cg_block_unresolved_owner ON cg_block_unresolved(owner_id, block_id);
CREATE VIRTUAL TABLE cg_fts_blocks USING fts5(name, path, tokenize='unicode61');
";

/// Read-only compatibility views with the v8 table names and column order.
const SCHEMA_V9_VIEWS: &str = "
CREATE VIEW cg_files AS
    SELECT m.workspace_id AS workspace_id, m.generation AS generation, b.path AS path,
           b.file_id AS file_id, b.source_digest AS source_digest
    FROM cg_manifest m JOIN cg_blocks b ON b.block_id=m.block_id
    WHERE b.kind='S';
CREATE VIEW cg_file_diagnostics AS
    SELECT m.workspace_id AS workspace_id, m.generation AS generation, b.path AS path,
           b.diagnostic AS diagnostic
    FROM cg_manifest m JOIN cg_blocks b ON b.block_id=m.block_id
    WHERE b.kind='S' AND b.diagnostic IS NOT NULL;
CREATE VIEW cg_nodes AS
    SELECT m.workspace_id AS workspace_id, m.generation AS generation, n.node_id AS node_id,
           n.node_key AS node_key, n.kind AS kind, n.name AS name, b.path AS path,
           n.start_byte AS start_byte, n.end_byte AS end_byte, n.start_line AS start_line,
           n.start_column AS start_column, n.end_line AS end_line, n.end_column AS end_column,
           n.provenance AS provenance
    FROM cg_manifest m JOIN cg_block_nodes n ON n.block_id=m.block_id
         JOIN cg_blocks b ON b.block_id=m.block_id;
CREATE VIEW cg_relations AS
    SELECT m.workspace_id AS workspace_id, m.generation AS generation, r.relation_id AS relation_id,
           r.relation_key AS relation_key, r.kind AS kind, r.source_id AS source_id,
           r.target_id AS target_id, r.provenance AS provenance
    FROM cg_manifest m JOIN cg_block_relations r ON r.block_id=m.block_id;
CREATE VIEW cg_evidence AS
    SELECT m.workspace_id AS workspace_id, m.generation AS generation, e.fact_id AS fact_id,
           e.ordinal AS ordinal, e.label AS label, e.path AS path, e.start_byte AS start_byte,
           e.end_byte AS end_byte, e.start_line AS start_line, e.start_column AS start_column,
           e.end_line AS end_line, e.end_column AS end_column, e.source_digest AS source_digest,
           e.evidence_digest AS evidence_digest, e.fact_type AS fact_type
    FROM cg_manifest m JOIN cg_block_evidence e ON e.block_id=m.block_id;
CREATE VIEW cg_unresolved_references AS
    SELECT m.workspace_id AS workspace_id, m.generation AS generation,
           u.unresolved_id AS unresolved_id, u.site_key AS site_key, u.kind AS kind,
           u.owner_id AS owner_id, u.raw_target AS raw_target, b.path AS path,
           u.start_byte AS start_byte, u.end_byte AS end_byte, u.start_line AS start_line,
           u.start_column AS start_column, u.end_line AS end_line, u.end_column AS end_column,
           u.source_digest AS source_digest, u.reference_digest AS reference_digest,
           u.provenance AS provenance
    FROM cg_manifest m JOIN cg_block_unresolved u ON u.block_id=m.block_id
         JOIN cg_blocks b ON b.block_id=m.block_id;
CREATE VIEW cg_fts_nodes AS
    SELECT f.name AS name, f.path AS path, m.workspace_id AS workspace_id,
           m.generation AS generation, n.node_id AS node_id, f.path AS owner_path
    FROM cg_fts_blocks f JOIN cg_block_nodes n ON n.node_ix=f.rowid
         JOIN cg_manifest m ON m.block_id=n.block_id;
";

/// The v10 table (#2351): which stored blocks a file's canonical staged
/// payload was published as, keyed by the file version and the payload's
/// digest. Staging a file whose payload is listed writes a reference, not the
/// facts. Rows go with the blocks they name.
const SCHEMA_V10_TABLES: &str = "
CREATE TABLE cg_payload_refs (
    path TEXT NOT NULL,
    source_digest TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    structure_block INTEGER NOT NULL,
    resolution_block INTEGER,
    PRIMARY KEY (path, source_digest, payload_digest)
) WITHOUT ROWID;
CREATE INDEX cg_payload_refs_structure ON cg_payload_refs(structure_block);
CREATE INDEX cg_payload_refs_resolution ON cg_payload_refs(resolution_block)
    WHERE resolution_block IS NOT NULL;
";

/// Stop copying per workspace (#2351). The index manifest is read from the
/// head's structure blocks and the identity ledger keeps only identities that
/// left every retained generation, so both go from one row per file / identity
/// per workspace to nothing for unchanged content. Returns whether pages were
/// freed. Runs inside the caller's migration transaction.
pub(crate) fn migrate_v9_to_v10(tx: &Transaction<'_>) -> Result<bool> {
    tx.execute_batch(&format!(
        "DROP TABLE IF EXISTS cg_payload_refs;{SCHEMA_V10_TABLES}"
    ))?;
    let mut dropped = tx.execute("DELETE FROM cg_retained_nodes WHERE tombstoned=0", [])?;
    dropped += tx.execute("DELETE FROM cg_retained_relations WHERE tombstoned=0", [])?;
    // A tombstone is exact only while no retained generation holds the identity.
    // #2930: CROSS JOIN pins the loop order: the identity's blocks by its index,
    // then the workspace's few ready generations, then one manifest primary-key
    // probe. Left to the planner, the subquery walked the workspace's whole
    // manifest (every generation x block) once per tombstone: on the hub's
    // 1.3 GB database (59k tombstones x 250k manifest rows) the migration ran for
    // hours, rolled back at every restart and held a core.
    dropped += tx.execute(
        "DELETE FROM cg_retained_nodes WHERE EXISTS (
            SELECT 1 FROM cg_block_nodes c
              CROSS JOIN cg_snapshots s
              CROSS JOIN cg_manifest m
            WHERE c.node_id=cg_retained_nodes.node_id
              AND s.workspace_id=cg_retained_nodes.workspace_id AND s.ready=1
              AND m.workspace_id=s.workspace_id
              AND m.generation=s.generation AND m.block_id=c.block_id)",
        [],
    )?;
    dropped += tx.execute(
        "DELETE FROM cg_retained_relations WHERE EXISTS (
            SELECT 1 FROM cg_block_relations c
              CROSS JOIN cg_snapshots s
              CROSS JOIN cg_manifest m
            WHERE c.relation_id=cg_retained_relations.relation_id
              AND s.workspace_id=cg_retained_relations.workspace_id AND s.ready=1
              AND m.workspace_id=s.workspace_id
              AND m.generation=s.generation AND m.block_id=c.block_id)",
        [],
    )?;
    // Absent when a database is reopened below v5 by a test fixture.
    let has_manifest: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='cg_index_manifest')",
        [],
        |row| row.get(0),
    )?;
    if has_manifest {
        dropped += tx.execute("DELETE FROM cg_index_manifest", [])?;
        tx.execute_batch("DROP TABLE cg_index_manifest;")?;
    }
    Ok(dropped > 0)
}

fn legacy_error(message: String) -> CodegraphError {
    CodegraphError::InvalidInput(message)
}

/// Convert every ready v8 generation into manifests of shared blocks, then
/// replace the v8 fact tables with compatibility views. Runs inside the
/// caller's migration transaction, so a failure leaves the v8 database intact.
/// Returns whether any generation was converted.
pub(crate) fn migrate_v8_to_v9(tx: &Transaction<'_>) -> Result<bool> {
    tx.execute_batch(SCHEMA_V9_TABLES)?;
    let generations = {
        let mut statement = tx.prepare(
            "SELECT workspace_id,generation FROM cg_snapshots WHERE ready=1
             ORDER BY workspace_id,generation",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (workspace, generation) in &generations {
        convert_generation(tx, workspace, *generation)?;
    }
    tx.execute_batch(
        "DROP TABLE cg_evidence;
         DROP TABLE cg_unresolved_references;
         DROP TABLE cg_file_diagnostics;
         DROP TABLE cg_relations;
         DROP TABLE cg_nodes;
         DROP TABLE cg_files;
         DROP TABLE cg_fts_nodes;",
    )?;
    tx.execute_batch(SCHEMA_V9_VIEWS)?;
    Ok(!generations.is_empty())
}

type FilePair = (BlockContent, BlockContent);

#[allow(clippy::too_many_lines)] // One generation's v8 rows are read, grouped and stored together.
fn convert_generation(tx: &Transaction<'_>, workspace: &str, generation: i64) -> Result<()> {
    let mut files: BTreeMap<String, FilePair> = BTreeMap::new();
    {
        let mut statement = tx.prepare(
            "SELECT path,file_id,source_digest FROM cg_files
             WHERE workspace_id=?1 AND generation=?2 ORDER BY path",
        )?;
        let rows = statement.query_map(params![workspace, generation], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (path, file_id, digest) = row?;
            files.insert(
                path.clone(),
                (
                    BlockContent::new(BlockKind::Structure, &path, &digest, Some(file_id), None),
                    BlockContent::new(BlockKind::Resolution, &path, &digest, None, None),
                ),
            );
        }
    }
    if files.is_empty() {
        return Ok(());
    }
    {
        let mut statement = tx.prepare(
            "SELECT path,diagnostic FROM cg_file_diagnostics
             WHERE workspace_id=?1 AND generation=?2",
        )?;
        let rows = statement.query_map(params![workspace, generation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (path, diagnostic) = row?;
            if let Some(pair) = files.get_mut(&path) {
                pair.0.diagnostic = Some(diagnostic);
            }
        }
    }
    let mut node_paths: HashMap<String, String> = HashMap::new();
    {
        let mut statement = tx.prepare(
            "SELECT node_id,node_key,kind,name,path,start_byte,end_byte,start_line,start_column,
                    end_line,end_column,provenance
             FROM cg_nodes WHERE workspace_id=?1 AND generation=?2",
        )?;
        let rows = statement.query_map(params![workspace, generation], |row| {
            let path: String = row.get(4)?;
            Ok(NodeRow {
                id: row.get(0)?,
                key: row.get(1)?,
                kind: row.get(2)?,
                name: row.get(3)?,
                span: SourceSpan {
                    path,
                    start_byte: row.get(5)?,
                    end_byte: row.get(6)?,
                    start_line: row.get(7)?,
                    start_column: row.get(8)?,
                    end_line: row.get(9)?,
                    end_column: row.get(10)?,
                },
                provenance: row.get(11)?,
            })
        })?;
        for row in rows {
            let row = row?;
            let pair = files.get_mut(&row.span.path).ok_or_else(|| {
                legacy_error(format!(
                    "legacy node {} names a file outside its snapshot",
                    row.id
                ))
            })?;
            node_paths.insert(row.id.clone(), row.span.path.clone());
            pair.0.nodes.push(row);
        }
    }
    let mut evidence: HashMap<String, Vec<EvidenceRow>> = HashMap::new();
    {
        let mut statement = tx.prepare(
            "SELECT fact_id,fact_type,ordinal,label,path,start_byte,end_byte,start_line,
                    start_column,end_line,end_column,source_digest,evidence_digest
             FROM cg_evidence WHERE workspace_id=?1 AND generation=?2 ORDER BY fact_id,ordinal",
        )?;
        let rows = statement.query_map(params![workspace, generation], |row| {
            let fact_id: String = row.get(0)?;
            Ok(EvidenceRow {
                fact_id,
                fact_type: row.get(1)?,
                ordinal: row.get(2)?,
                label: row.get(3)?,
                span: SourceSpan {
                    path: row.get(4)?,
                    start_byte: row.get(5)?,
                    end_byte: row.get(6)?,
                    start_line: row.get(7)?,
                    start_column: row.get(8)?,
                    end_line: row.get(9)?,
                    end_column: row.get(10)?,
                },
                source_digest: row.get(11)?,
                evidence_digest: row.get(12)?,
            })
        })?;
        for row in rows {
            let row = row?;
            evidence.entry(row.fact_id.clone()).or_default().push(row);
        }
    }
    for pair in files.values_mut() {
        let ids = pair
            .0
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(items) = evidence.remove(&id) {
                pair.0.evidence.extend(items);
            }
        }
    }
    let fallback = files
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| legacy_error("legacy snapshot has no files".into()))?;
    {
        let mut statement = tx.prepare(
            "SELECT relation_id,relation_key,kind,source_id,target_id,provenance
             FROM cg_relations WHERE workspace_id=?1 AND generation=?2",
        )?;
        let rows = statement.query_map(params![workspace, generation], |row| {
            Ok(RelationRow {
                id: row.get(0)?,
                key: row.get(1)?,
                kind: row.get(2)?,
                source: row.get(3)?,
                target: row.get(4)?,
                provenance: row.get(5)?,
            })
        })?;
        for row in rows {
            let row = row?;
            // A v8 relation row does not record its owner file: the first
            // evidence span's file is the owner the extractor validated, and the
            // source node's file is the fallback.
            let items = evidence.remove(&row.id).unwrap_or_default();
            let owner = items
                .first()
                .map(|item| item.span.path.clone())
                .filter(|path| files.contains_key(path))
                .or_else(|| node_paths.get(&row.source).cloned())
                .unwrap_or_else(|| fallback.clone());
            let pair = files.get_mut(&owner).ok_or_else(|| {
                legacy_error(format!("legacy relation {} has no owner file", row.id))
            })?;
            let block = if is_structural_kind(&row.kind) {
                &mut pair.0
            } else {
                &mut pair.1
            };
            block.evidence.extend(items);
            block.relations.push(row);
        }
    }
    {
        let mut statement = tx.prepare(
            "SELECT site_key,kind,owner_id,raw_target,path,start_byte,end_byte,start_line,
                    start_column,end_line,end_column,source_digest,reference_digest,provenance
             FROM cg_unresolved_references WHERE workspace_id=?1 AND generation=?2",
        )?;
        let rows = statement.query_map(params![workspace, generation], |row| {
            let site_key: String = row.get(0)?;
            Ok(UnresolvedRow {
                id: unresolved_id(&site_key).to_string(),
                site_key,
                kind: row.get(1)?,
                owner: row.get(2)?,
                raw_target: row.get(3)?,
                span: SourceSpan {
                    path: row.get(4)?,
                    start_byte: row.get(5)?,
                    end_byte: row.get(6)?,
                    start_line: row.get(7)?,
                    start_column: row.get(8)?,
                    end_line: row.get(9)?,
                    end_column: row.get(10)?,
                },
                source_digest: row.get(11)?,
                reference_digest: row.get(12)?,
                provenance: row.get(13)?,
            })
        })?;
        for row in rows {
            let row = row?;
            let pair = files.get_mut(&row.span.path).ok_or_else(|| {
                legacy_error(format!(
                    "legacy unresolved site {} names a file outside its snapshot",
                    row.site_key
                ))
            })?;
            pair.1.unresolved.push(row);
        }
    }
    for (_, (structure, resolution)) in files {
        store_file_blocks(
            tx,
            workspace,
            generation,
            (structure, (!resolution.is_empty()).then_some(resolution)),
        )?;
    }
    Ok(())
}

/// Counters for the shared store, read without scanning fact rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageSummary {
    /// Distinct stored blocks.
    pub blocks: u64,
    /// Manifest rows: one per block per generation that references it.
    pub manifest_rows: u64,
    /// Estimated bytes of the stored blocks, each counted once.
    pub block_bytes: u64,
}

/// Check the reference counts against the manifests.
pub(crate) fn verify_references(connection: &rusqlite::Connection) -> Result<()> {
    let mut mismatched = connection.prepare(
        "SELECT b.block_id, b.refs, (SELECT count(*) FROM cg_manifest m WHERE m.block_id=b.block_id)
         FROM cg_blocks b",
    )?;
    let rows = mismatched.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (block_id, refs, actual) = row?;
        if refs != actual {
            return Err(CodegraphError::InvalidInput(format!(
                "block {block_id} records {refs} references but {actual} manifests use it"
            )));
        }
    }
    let dangling: i64 = connection.query_row(
        "SELECT count(*) FROM cg_manifest m
         WHERE NOT EXISTS (SELECT 1 FROM cg_blocks b WHERE b.block_id=m.block_id)",
        [],
        |row| row.get(0),
    )?;
    if dangling != 0 {
        return Err(CodegraphError::InvalidInput(format!(
            "{dangling} manifest rows name a missing block"
        )));
    }
    Ok(())
}

pub(crate) fn summary(connection: &rusqlite::Connection) -> Result<StorageSummary> {
    let (blocks, bytes): (i64, i64) = connection.query_row(
        "SELECT count(*), COALESCE(sum(bytes),0) FROM cg_blocks",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let manifest_rows: i64 =
        connection.query_row("SELECT count(*) FROM cg_manifest", [], |row| row.get(0))?;
    let count = |value: i64| u64::try_from(value).unwrap_or(0);
    Ok(StorageSummary {
        blocks: count(blocks),
        manifest_rows: count(manifest_rows),
        block_bytes: count(bytes),
    })
}

/// Keep a set of 128-bit fingerprints for duplicate-key checks without holding
/// every key string for a repository-sized publication.
#[derive(Default)]
pub(crate) struct KeySet(HashSet<u128>);

impl KeySet {
    /// Returns false when the key was already present.
    pub(crate) fn insert(&mut self, key: &str) -> bool {
        let hash = blake3::hash(key.as_bytes());
        let mut head = [0u8; 16];
        head.copy_from_slice(&hash.as_bytes()[..16]);
        self.0.insert(u128::from_be_bytes(head))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use uuid::Uuid;

    use crate::CodegraphStore;

    /// #2930: seed `blocks` one-node, one-relation blocks over two ready
    /// generations (generation 1 holds every block, generation 2 the second
    /// half), a tombstone for every identity a block holds and as many for
    /// identities no block holds, then run the v9 -> v10 ledger cleanup.
    /// Returns the SQLite VM instructions it took and the node and relation
    /// tombstones it kept.
    fn v10_ledger_cleanup_cost(blocks: i64) -> (u64, i64, i64) {
        let root = tempfile::tempdir().unwrap();
        let mut store =
            CodegraphStore::open(root.path().join("codegraph.sqlite"), Uuid::new_v4()).unwrap();
        let id = Uuid::new_v4().to_string();
        store
            .connection
            .execute_batch(&format!(
                "WITH RECURSIVE k(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM k WHERE i < {blocks})
                 INSERT INTO cg_blocks(block_id,kind,digest,path,source_digest,bytes,refs)
                   SELECT i,'S','d'||i,'p'||i,'s'||i,1,1 FROM k;
                 INSERT INTO cg_block_nodes(block_id,node_id,node_key,kind,name,start_byte,end_byte,
                     start_line,start_column,end_line,end_column,provenance)
                   SELECT block_id,'n'||block_id,'k'||block_id,'fn','f'||block_id,0,1,0,0,0,1,'Extracted'
                   FROM cg_blocks;
                 INSERT INTO cg_block_relations(block_id,relation_id,relation_key,kind,source_id,target_id,provenance)
                   SELECT block_id,'r'||block_id,'rk'||block_id,'calls','n'||block_id,'n1','Extracted'
                   FROM cg_blocks;
                 INSERT INTO cg_snapshots(project_id,repository_id,workspace_id,generation,
                     snapshot_digest,graph_digest,ready)
                   VALUES('p','r','{id}',1,'a','b',1),('p','r','{id}',2,'a','b',1);
                 INSERT INTO cg_workspace_heads(workspace_id,generation) VALUES('{id}',2);
                 INSERT INTO cg_manifest SELECT '{id}',1,block_id FROM cg_blocks;
                 INSERT INTO cg_manifest SELECT '{id}',2,block_id FROM cg_blocks
                   WHERE block_id>{half};
                 INSERT INTO cg_retained_nodes(workspace_id,node_id,node_key,kind,name,
                     last_detailed_generation,tombstoned)
                   SELECT '{id}',node_id,node_key,kind,name,1,1 FROM cg_block_nodes
                   UNION ALL SELECT '{id}','gone'||block_id,'g','fn','g',1,1 FROM cg_blocks;
                 INSERT INTO cg_retained_relations(workspace_id,relation_id,relation_key,kind,
                     source_id,target_id,last_detailed_generation,tombstoned)
                   SELECT '{id}',relation_id,relation_key,kind,source_id,target_id,1,1
                   FROM cg_block_relations
                   UNION ALL SELECT '{id}','gone'||block_id,'g','calls','a','b',1,1 FROM cg_blocks;",
                half = blocks / 2,
            ))
            .unwrap();
        let counter = Arc::new(AtomicU64::new(0));
        let handle = Arc::clone(&counter);
        store.connection.progress_handler(
            1,
            Some(move || {
                handle.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let transaction = store.connection.transaction().unwrap();
        super::migrate_v9_to_v10(&transaction).unwrap();
        transaction.commit().unwrap();
        store.connection.progress_handler(1, None::<fn() -> bool>);
        let kept = |table: &str| -> i64 {
            store
                .connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        (
            counter.load(Ordering::Relaxed),
            kept("cg_retained_nodes"),
            kept("cg_retained_relations"),
        )
    }

    /// #2930: the v10 ledger cleanup is linear in the tombstones, not
    /// tombstones x manifest rows. Quadrupling the blocks (and with them the
    /// tombstones and the manifest) must not grow the work 16x, and the cleanup
    /// stays exact: only tombstones of identities no ready generation holds stay.
    #[test]
    fn v10_ledger_cleanup_is_linear_in_the_tombstones_2930() {
        let (small, small_nodes, small_relations) = v10_ledger_cleanup_cost(100);
        let (large, large_nodes, large_relations) = v10_ledger_cleanup_cost(400);
        assert_eq!((small_nodes, small_relations), (100, 100));
        assert_eq!((large_nodes, large_relations), (400, 400));
        assert!(
            large < small * 6,
            "v10 cleanup cost grew {large} / {small} for 4x the blocks: not linear"
        );
        assert!(
            large < 1_600 * 400,
            "{large} VM instructions for 1600 tombstones: a per-tombstone walk of the manifest"
        );
    }
}
