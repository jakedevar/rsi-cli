//! Disk-backed, source-owned S1 publication. A run contains the complete current
//! file inventory; omitted owners have no rows in its next ready generation.

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CodegraphError, CodegraphStore, ExtractionContract, FactKey, NodeFact, PublishFault,
    ReadySnapshot, RelationFact, Result, SourceFactBundle, SourceFile, UnresolvedReferenceFact,
    WorkspaceScope, blocks, evidence_order, node_id, parse_extraction_mode,
    validate_bundle_with_limits, validate_path,
};

/// Largest complete inventory one run may declare. The RSI workspace already
/// declares about 9,000 owners, so this leaves room to grow (#2323).
pub const MAX_STAGED_FILES: usize = 50_000;
pub const MAX_STAGED_FILE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STAGED_SOURCE_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_STAGED_FACTS_PER_FILE: usize = 50_000;
pub const MAX_STAGED_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Maximum charged source and encoded fact buffers for one adapter call.
pub const MAX_STAGED_BUFFER_BYTES: usize = 24 * 1024 * 1024;
/// Sanity valve on the encoded facts of one run, sized from the largest source
/// set discovery admits: encoded facts are an expansion of source text, so
/// eight times the source ceiling is far above any real workspace. It is not
/// the disk bound (the physical write budget is). The total fact count has no
/// ceiling at all: staging is disk-backed, `publish_staged` reads one file at a
/// time, and the per-file fact and payload ceilings bound memory (#2323).
pub const MAX_STAGED_TOTAL_PAYLOAD_BYTES: usize = 8 * MAX_STAGED_SOURCE_BYTES;
pub const MAX_STAGED_SECONDS: i64 = 3_600;
pub const PARSER_MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const SOURCE_DIGEST_DOMAIN: &[u8] = b"blake3-hex-v1";

/// The extractor's coverage of one exact file version. Parse-error files may
/// carry parser-proven partial facts; other degraded files carry none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileDiagnostic {
    Parsed,
    ParseErrors { count: usize },
    Unsupported { reason: String },
    Oversize { bytes: usize },
    NonUtf8,
}

impl FileDiagnostic {
    const fn is_parsed(&self) -> bool {
        matches!(self, Self::Parsed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerFileFacts {
    pub file: SourceFile,
    pub nodes: Vec<NodeFact>,
    pub relations: Vec<RelationFact>,
    pub unresolved_references: Vec<UnresolvedReferenceFact>,
    pub diagnostic: FileDiagnostic,
    /// Syntax-local Rust facts for the resolver; never staged or persisted.
    pub scope: crate::scope::RustScope,
}

/// A discovered owner and its exact expected source version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceVersion {
    pub relative_path: String,
    pub source_digest: String,
}

/// Additional deterministic extractor inputs for the S1 digest domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedExtraction {
    pub extraction: ExtractionContract,
    pub grammar_version: String,
    pub rule_version: String,
    pub normalization_version: String,
    pub config_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageRun {
    pub workspace_id: Uuid,
    pub run_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageCompleteness {
    pub total_files: usize,
    pub parsed_files: usize,
    pub degraded_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFacts {
    nodes: Vec<NodeFact>,
    relations: Vec<RelationFact>,
    unresolved_references: Vec<UnresolvedReferenceFact>,
    diagnostic: FileDiagnostic,
}

fn ensure_deadline(started: DateTime<Utc>) -> Result<()> {
    let elapsed = Utc::now()
        .signed_duration_since(started)
        .num_seconds()
        .max(0);
    limit(
        "run seconds",
        checked_count(elapsed)?,
        checked_count(MAX_STAGED_SECONDS)?,
    )
}

fn checked_version(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 128 || value.contains('\0') {
        return Err(CodegraphError::InvalidInput(format!(
            "{field} must be nonempty and at most 128 bytes"
        )));
    }
    Ok(())
}

fn stage_run_started(transaction: &Transaction<'_>, run: &StageRun) -> Result<DateTime<Utc>> {
    let started: Option<String> = transaction
        .query_row(
            "SELECT started_at FROM cg_stage_runs WHERE workspace_id=?1 AND run_id=?2",
            params![run.workspace_id.to_string(), run.run_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let started = started.ok_or_else(|| {
        CodegraphError::InvalidInput("stage run was replaced or does not exist".into())
    })?;
    DateTime::parse_from_rfc3339(&started)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| CodegraphError::InvalidInput("invalid stage start time".into()))
}

/// Check one staging ceiling. The quantity is named so a failed index run
/// reports which ceiling it hit, not only the numbers (#2323).
const fn limit(quantity: &'static str, value: usize, maximum: usize) -> Result<()> {
    if value > maximum {
        return Err(CodegraphError::StageCeilingExceeded {
            quantity,
            requested: value,
            maximum,
        });
    }
    Ok(())
}

fn checked_count(value: i64) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| CodegraphError::InvalidInput(format!("invalid staged count: {value}")))
}

fn decode(payload: &[u8]) -> Result<StoredFacts> {
    serde_json::from_slice(payload)
        .map_err(|error| CodegraphError::InvalidInput(format!("invalid staged facts: {error}")))
}

/// Marks a staged payload that points at stored blocks instead of holding the
/// file's facts (#2351). Facts payloads are JSON objects and start with `{`.
const REF_MAGIC: &[u8] = b"rsi-cg-ref1\n";

#[derive(Debug, Serialize, Deserialize)]
struct StoredRef {
    payload_digest: String,
    structure_block: i64,
    resolution_block: Option<i64>,
}

fn encode_ref(reference: &blocks::PayloadRef) -> Result<Vec<u8>> {
    let mut payload = REF_MAGIC.to_vec();
    serde_json::to_writer(
        &mut payload,
        &StoredRef {
            payload_digest: reference.payload_digest.clone(),
            structure_block: reference.blocks.structure,
            resolution_block: reference.blocks.resolution,
        },
    )
    .map_err(|error| CodegraphError::InvalidInput(format!("cannot encode reference: {error}")))?;
    Ok(payload)
}

fn decode_ref(path: &str, source_digest: &str, payload: &[u8]) -> Result<blocks::PayloadRef> {
    let stored: StoredRef =
        serde_json::from_slice(&payload[REF_MAGIC.len()..]).map_err(|error| {
            CodegraphError::InvalidInput(format!("invalid staged reference: {error}"))
        })?;
    Ok(blocks::PayloadRef {
        path: path.to_owned(),
        source_digest: source_digest.to_owned(),
        payload_digest: stored.payload_digest,
        blocks: blocks::FileBlocks {
            structure: stored.structure_block,
            resolution: stored.resolution_block,
        },
    })
}

/// A staged file: its facts, or the stored blocks that already hold them.
enum StagedRow {
    Full {
        facts: StoredFacts,
        payload_digest: String,
    },
    Ref(blocks::PayloadRef),
}

/// Digest of a staged payload: the canonical facts' digest, whether the row
/// holds them or points at the blocks that do.
fn row_payload_digest(path: &str, payload: &[u8]) -> Result<String> {
    if payload.starts_with(REF_MAGIC) {
        Ok(decode_ref(path, "", payload)?.payload_digest)
    } else {
        Ok(blake3::hash(payload).to_hex().to_string())
    }
}

/// One staged row with its recorded source digest.
fn staged_row(
    transaction: &Transaction<'_>,
    workspace: Uuid,
    path: &str,
) -> Result<(String, StagedRow)> {
    let (source_digest, payload): (String, Vec<u8>) = transaction.query_row(
        "SELECT source_digest,payload FROM cg_stage_files WHERE workspace_id=?1 AND path=?2",
        params![workspace.to_string(), path],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    limit(
        "file payload bytes",
        payload.len(),
        MAX_STAGED_PAYLOAD_BYTES,
    )?;
    let row = if payload.starts_with(REF_MAGIC) {
        StagedRow::Ref(decode_ref(path, &source_digest, &payload)?)
    } else {
        StagedRow::Full {
            facts: decode(&payload)?,
            payload_digest: blake3::hash(&payload).to_hex().to_string(),
        }
    };
    Ok((source_digest, row))
}

fn diagnostic_of(stored: Option<&str>) -> Result<FileDiagnostic> {
    match stored {
        Some(text) => serde_json::from_str(text)
            .map_err(|error| CodegraphError::InvalidInput(error.to_string())),
        None => Ok(FileDiagnostic::Parsed),
    }
}

fn paths(transaction: &Transaction<'_>, workspace: Uuid) -> Result<Vec<String>> {
    let mut statement = transaction
        .prepare("SELECT path FROM cg_stage_files WHERE workspace_id=?1 ORDER BY path")?;
    let paths = statement
        .query_map([workspace.to_string()], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    limit("files", paths.len(), MAX_STAGED_FILES)?;
    Ok(paths)
}

fn fact_count(facts: &PerFileFacts) -> usize {
    facts
        .nodes
        .len()
        .saturating_add(facts.relations.len())
        .saturating_add(facts.unresolved_references.len())
        .saturating_add(
            facts
                .nodes
                .iter()
                .map(|node| node.evidence.len())
                .sum::<usize>(),
        )
        .saturating_add(
            facts
                .relations
                .iter()
                .map(|relation| relation.evidence.len())
                .sum::<usize>(),
        )
}

fn canonical_facts(facts: &PerFileFacts) -> StoredFacts {
    let mut nodes = facts.nodes.clone();
    nodes.sort_by(|left, right| left.key.0.cmp(&right.key.0));
    for node in &mut nodes {
        node.evidence.sort_by(evidence_order);
    }
    let mut relations = facts.relations.clone();
    relations.sort_by(|left, right| left.key.0.cmp(&right.key.0));
    for relation in &mut relations {
        relation.evidence.sort_by(evidence_order);
    }
    let mut unresolved_references = facts.unresolved_references.clone();
    unresolved_references.sort_by(|left, right| left.key.0.cmp(&right.key.0));
    StoredFacts {
        nodes,
        relations,
        unresolved_references,
        diagnostic: facts.diagnostic.clone(),
    }
}

fn validate_file(extraction: ExtractionContract, facts: &PerFileFacts) -> Result<()> {
    let count = fact_count(facts);
    limit("facts in one file", count, MAX_STAGED_FACTS_PER_FILE)?;
    if facts.file.bytes.len() > PARSER_MAX_FILE_BYTES
        && !matches!(facts.diagnostic, FileDiagnostic::Oversize { .. })
    {
        return Err(CodegraphError::InvalidInput(
            "source above parser threshold requires Oversize diagnostic".into(),
        ));
    }
    match &facts.diagnostic {
        FileDiagnostic::Parsed => {
            if std::str::from_utf8(&facts.file.bytes).is_err() {
                return Err(CodegraphError::InvalidInput(
                    "parsed source must be UTF-8".into(),
                ));
            }
        }
        FileDiagnostic::ParseErrors { count } if *count > 0 => {}
        FileDiagnostic::ParseErrors { .. } => {
            return Err(CodegraphError::InvalidInput(
                "parse error count must be positive".into(),
            ));
        }
        FileDiagnostic::Unsupported { reason }
            if !reason.trim().is_empty() && reason.len() <= 256 => {}
        FileDiagnostic::Unsupported { .. } => {
            return Err(CodegraphError::InvalidInput(
                "unsupported reason must be nonempty and bounded".into(),
            ));
        }
        FileDiagnostic::Oversize { bytes }
            if *bytes == facts.file.bytes.len() && *bytes > PARSER_MAX_FILE_BYTES => {}
        FileDiagnostic::Oversize { .. } => {
            return Err(CodegraphError::InvalidInput(
                "oversize diagnostic requires source above parser threshold and exact byte count"
                    .into(),
            ));
        }
        FileDiagnostic::NonUtf8 => {}
    }
    if matches!(facts.diagnostic, FileDiagnostic::ParseErrors { .. }) {
        let expected = crate::extract::extract_file(facts.file.clone());
        if expected != *facts {
            return Err(CodegraphError::InvalidInput(
                "parse-error diagnostic and facts must match the exact-source parser projection"
                    .into(),
            ));
        }
    } else if !facts.diagnostic.is_parsed() && count != 0 {
        return Err(CodegraphError::InvalidInput(
            "degraded extraction cannot submit structural facts".into(),
        ));
    }
    if facts
        .nodes
        .iter()
        .any(|node| node.span.path != facts.file.relative_path)
        || facts
            .relations
            .iter()
            .any(|relation| relation.owner_file != facts.file.relative_path)
        || facts
            .unresolved_references
            .iter()
            .any(|reference| reference.span.path != facts.file.relative_path)
    {
        return Err(CodegraphError::InvalidInput(
            "per-file fact must be owned by its source path".into(),
        ));
    }
    let bundle = SourceFactBundle {
        extraction,
        files: vec![facts.file.clone()],
        nodes: facts.nodes.clone(),
        relations: facts.relations.clone(),
        unresolved_references: facts.unresolved_references.clone(),
    };
    validate_bundle_with_limits(
        &bundle,
        1,
        MAX_STAGED_FACTS_PER_FILE,
        MAX_STAGED_FILE_BYTES,
        MAX_STAGED_FILE_BYTES,
        false,
    )
}

impl CodegraphStore {
    /// Start a complete-inventory run. Every discovered path must be named
    /// here; a new run replaces only staging rows, never the last ready head.
    ///
    /// # Errors
    /// Returns invalid scope/input or `SQLite` errors.
    #[allow(clippy::too_many_lines)] // The scope, manifest, and inventory must enter one atomic run.
    pub fn begin_staged(
        &mut self,
        scope: &WorkspaceScope,
        manifest: &StagedExtraction,
        owners: &[SourceVersion],
    ) -> Result<StageRun> {
        let expected = self.scope(scope.instance_key.clone());
        if &expected != scope {
            return Err(CodegraphError::InvalidInput(
                "workspace scope is not bound to this store".into(),
            ));
        }
        if !manifest.extraction.mode.is_publishable() {
            return Err(CodegraphError::InvalidInput(
                "staging requires EXTRACTED/1.0 or EXTRACTED+INFERRED/1.1".into(),
            ));
        }
        for (name, value) in [
            (
                "extractor name",
                manifest.extraction.extractor.name.as_str(),
            ),
            (
                "extractor version",
                manifest.extraction.extractor.version.as_str(),
            ),
            ("grammar version", manifest.grammar_version.as_str()),
            ("rule version", manifest.rule_version.as_str()),
            (
                "normalization version",
                manifest.normalization_version.as_str(),
            ),
            ("config digest", manifest.config_digest.as_str()),
        ] {
            checked_version(value, name)?;
        }
        if owners.is_empty() {
            return Err(CodegraphError::InvalidInput(
                "stage inventory has no source files".into(),
            ));
        }
        limit("files", owners.len(), MAX_STAGED_FILES)?;
        let mut unique_paths = std::collections::HashSet::with_capacity(owners.len());
        for owner in owners {
            validate_path(&owner.relative_path)?;
            if !unique_paths.insert(&owner.relative_path) {
                return Err(CodegraphError::InvalidInput(format!(
                    "duplicate stage owner: {}",
                    owner.relative_path
                )));
            }
            if owner.source_digest.len() != 64
                || !owner
                    .source_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(CodegraphError::InvalidInput(format!(
                    "invalid source digest for {}",
                    owner.relative_path
                )));
            }
        }
        let run = StageRun {
            workspace_id: scope.workspace_id,
            run_id: Uuid::new_v4(),
        };
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM cg_stage_files WHERE workspace_id=?1",
            [run.workspace_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM cg_stage_inventory WHERE workspace_id=?1",
            [run.workspace_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM cg_stage_runs WHERE workspace_id=?1",
            [run.workspace_id.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO cg_stage_runs(workspace_id,run_id,started_at,extraction_mode,
             extractor_name,extractor_version,grammar_version,rule_version,
             normalization_version,config_digest) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                run.workspace_id.to_string(),
                run.run_id.to_string(),
                Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
                manifest.extraction.mode.as_str(),
                manifest.extraction.extractor.name,
                manifest.extraction.extractor.version,
                manifest.grammar_version,
                manifest.rule_version,
                manifest.normalization_version,
                manifest.config_digest
            ],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO cg_stage_inventory(workspace_id,path,source_digest) VALUES (?1,?2,?3)",
            )?;
            for owner in owners {
                insert.execute(params![
                    run.workspace_id.to_string(),
                    owner.relative_path,
                    owner.source_digest
                ])?;
            }
        }
        transaction.commit()?;
        Ok(run)
    }

    /// Upsert one exact owner file and its bounded facts. The run remains
    /// invisible to ready queries until `publish_staged` succeeds.
    ///
    /// # Errors
    /// Returns a visible bound, validation, stale-run, or `SQLite` error.
    pub fn stage_file(&mut self, run: &StageRun, facts: &PerFileFacts) -> Result<()> {
        validate_path(&facts.file.relative_path)?;
        limit(
            "file source bytes",
            facts.file.bytes.len(),
            MAX_STAGED_FILE_BYTES,
        )?;
        let transaction = self.connection.transaction()?;
        let started = stage_run_started(&transaction, run)?;
        ensure_deadline(started)?;
        let declared: Option<String> = transaction
            .query_row(
                "SELECT source_digest FROM cg_stage_inventory WHERE workspace_id=?1 AND path=?2",
                params![run.workspace_id.to_string(), facts.file.relative_path],
                |row| row.get(0),
            )
            .optional()?;
        let declared = declared.ok_or_else(|| {
            CodegraphError::InvalidInput(format!(
                "source path is outside the declared stage inventory: {}",
                facts.file.relative_path
            ))
        })?;
        let source_digest = blake3::hash(&facts.file.bytes).to_hex().to_string();
        if source_digest != declared {
            return Err(CodegraphError::InvalidInput(format!(
                "source bytes changed after inventory discovery: {}",
                facts.file.relative_path
            )));
        }
        let extraction = stage_extraction(&transaction, run)?;
        validate_file(extraction, facts)?;
        let stored = canonical_facts(facts);
        let payload = serde_json::to_vec(&stored).map_err(|error| {
            CodegraphError::InvalidInput(format!("cannot encode facts: {error}"))
        })?;
        limit(
            "file payload bytes",
            payload.len(),
            MAX_STAGED_PAYLOAD_BYTES,
        )?;
        limit(
            "file buffer bytes",
            facts.file.bytes.len().saturating_add(payload.len()),
            MAX_STAGED_BUFFER_BYTES,
        )?;
        // A file whose canonical facts are already stored as blocks is staged
        // as a reference to them (#2351): an unchanged corpus then writes a few
        // dozen bytes per file instead of its whole fact payload.
        let payload_digest = blake3::hash(&payload).to_hex().to_string();
        let payload = match blocks::find_payload_ref(
            &transaction,
            &facts.file.relative_path,
            &source_digest,
            &payload_digest,
        )? {
            Some(reference) => encode_ref(&reference)?,
            None => payload,
        };
        let old: Option<(i64, i64, i64)> = transaction.query_row(
            "SELECT source_bytes,fact_count,length(payload) FROM cg_stage_files WHERE workspace_id=?1 AND path=?2",
            params![run.workspace_id.to_string(), facts.file.relative_path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let (files, source, facts_total, payload_total): (i64, i64, i64, i64) = transaction.query_row(
            "SELECT file_count,source_bytes,fact_count,payload_bytes FROM cg_stage_runs WHERE workspace_id=?1 AND run_id=?2",
            params![run.workspace_id.to_string(), run.run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let is_new = old.is_none();
        let previous = old.unwrap_or((0, 0, 0));
        let next_files = checked_count(files + i64::from(is_new))?;
        let next_source =
            checked_count(source - previous.0)?.saturating_add(facts.file.bytes.len());
        let next_facts = checked_count(facts_total - previous.1)?.saturating_add(fact_count(facts));
        let next_payload = checked_count(payload_total - previous.2)?.saturating_add(payload.len());
        limit("files", next_files, MAX_STAGED_FILES)?;
        limit("source bytes", next_source, MAX_STAGED_SOURCE_BYTES)?;
        limit(
            "total payload bytes",
            next_payload,
            MAX_STAGED_TOTAL_PAYLOAD_BYTES,
        )?;
        transaction.execute(
            "INSERT INTO cg_stage_files VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(workspace_id,path) DO UPDATE SET source_digest=excluded.source_digest,
             source_bytes=excluded.source_bytes,fact_count=excluded.fact_count,payload=excluded.payload",
            params![run.workspace_id.to_string(), facts.file.relative_path,
                source_digest, facts.file.bytes.len(),
                fact_count(facts), payload],
        )?;
        let updated = transaction.execute(
            "UPDATE cg_stage_runs SET file_count=?3,source_bytes=?4,fact_count=?5,payload_bytes=?6
             WHERE workspace_id=?1 AND run_id=?2",
            params![
                run.workspace_id.to_string(),
                run.run_id.to_string(),
                next_files,
                next_source,
                next_facts,
                next_payload
            ],
        )?;
        if updated != 1 {
            return Err(CodegraphError::InvalidInput(
                "stage run counter row disappeared before update".into(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Atomically publish a ready generation from every staged owner.
    ///
    /// # Errors
    /// Any validation, bound, missing endpoint, or injected failure rolls back
    /// all provisional rows and preserves the previous head. A competing WAL
    /// writer returns `RetryableWriterConflict`; retry the whole call before
    /// the original run deadline, never only a failed SQL statement.
    pub fn publish_staged(&mut self, run: &StageRun, fault: PublishFault) -> Result<ReadySnapshot> {
        self.publish_staged_once(run, fault)
            .map_err(retryable_writer_conflict)
    }

    #[allow(clippy::too_many_lines)] // Three ordered passes keep one atomic source-owned publication.
    fn publish_staged_once(
        &mut self,
        run: &StageRun,
        fault: PublishFault,
    ) -> Result<ReadySnapshot> {
        let project = self.project_id;
        let repository = Self::repository_id(project);
        let transaction = self.connection.transaction()?;
        let started = stage_run_started(&transaction, run)?;
        ensure_deadline(started)?;
        let owner_paths = paths(&transaction, run.workspace_id)?;
        if owner_paths.is_empty() {
            return Err(CodegraphError::InvalidInput(
                "stage run has no source files".into(),
            ));
        }
        let expected: i64 = transaction.query_row(
            "SELECT count(*) FROM cg_stage_inventory WHERE workspace_id=?1",
            [run.workspace_id.to_string()],
            |row| row.get(0),
        )?;
        if owner_paths.len() != checked_count(expected)? {
            return Err(CodegraphError::InvalidInput(format!(
                "stage inventory incomplete: expected {expected} files, staged {}",
                owner_paths.len()
            )));
        }
        verify_stage_counters(&transaction, run, owner_paths.len())?;
        let extraction = stage_extraction(&transaction, run)?;
        let (snapshot_digest, graph_digest) = staged_digests(
            &transaction,
            run,
            project,
            repository,
            &owner_paths,
            SOURCE_DIGEST_DOMAIN,
        )?;
        // Pass 1 learns every node identity, so a relation in one file can name
        // a node in another. Only the key and identity of each node are kept.
        // A file staged as a reference contributes the nodes of its stored block.
        let mut node_ids: std::collections::HashMap<String, Uuid> =
            std::collections::HashMap::new();
        let mut parsed = 0;
        let mut references: Vec<blocks::PayloadRef> = Vec::new();
        let mut stale: Vec<blocks::PayloadRef> = Vec::new();
        for path in &owner_paths {
            ensure_deadline(started)?;
            let (_, row) = staged_row(&transaction, run.workspace_id, path)?;
            let nodes: Vec<(String, Uuid)> = match row {
                StagedRow::Full { facts: stored, .. } => {
                    parsed += usize::from(stored.diagnostic.is_parsed());
                    stored
                        .nodes
                        .iter()
                        .map(|node| (node.key.0.clone(), node_id(repository, node)))
                        .collect()
                }
                StagedRow::Ref(reference) => {
                    if !blocks::payload_ref_blocks_exist(&transaction, &reference)? {
                        stale.push(reference);
                        continue;
                    }
                    let diagnostic = diagnostic_of(
                        blocks::referenced_diagnostic(&transaction, &reference)?.as_deref(),
                    )?;
                    parsed += usize::from(diagnostic.is_parsed());
                    let nodes = blocks::referenced_nodes(&transaction, &reference)?;
                    references.push(reference);
                    nodes
                }
            };
            for (key, id) in nodes {
                if node_ids.insert(key.clone(), id).is_some() {
                    return Err(CodegraphError::InvalidInput(format!(
                        "duplicate node_key {key}"
                    )));
                }
            }
        }
        if stale.is_empty() {
            let nodes: std::collections::HashSet<Uuid> = node_ids.values().copied().collect();
            for reference in &references {
                if !blocks::referenced_endpoints_resolve(&transaction, reference, &nodes)? {
                    stale.push(reference.clone());
                }
            }
        }
        if !stale.is_empty() {
            // The blocks a reference names were reclaimed, or a node they relate
            // to is gone. Forget the references and have the caller stage the
            // run again from facts; nothing else was written yet.
            for reference in &stale {
                blocks::drop_payload_ref(&transaction, reference)?;
            }
            transaction.commit()?;
            return Err(CodegraphError::RetryableWriterConflict);
        }
        let head: Option<i64> = transaction
            .query_row(
                "SELECT generation FROM cg_workspace_heads WHERE workspace_id=?1",
                [run.workspace_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let generation = head.unwrap_or(0) + 1;
        transaction.execute(
            "INSERT INTO cg_snapshots(project_id,repository_id,workspace_id,generation,snapshot_digest,graph_digest,ready,extraction_mode,extractor_name,extractor_version)
             VALUES (?1,?2,?3,?4,?5,?6,0,?7,?8,?9)",
            params![project.to_string(), repository.to_string(), run.workspace_id.to_string(), generation,
                snapshot_digest, graph_digest, extraction.mode.as_str(), extraction.extractor.name,
                extraction.extractor.version],
        )?;
        // Pass 2 turns each file into content-addressed blocks. A file whose
        // facts match a stored block adds a manifest row and no fact rows.
        let workspace_text = run.workspace_id.to_string();
        let mut relation_keys = blocks::KeySet::default();
        let mut site_keys = blocks::KeySet::default();
        for path in &owner_paths {
            ensure_deadline(started)?;
            let (source_digest, row) = staged_row(&transaction, run.workspace_id, path)?;
            match row {
                StagedRow::Ref(reference) => {
                    if let Some(repeated) = blocks::referenced_keys(
                        &transaction,
                        &reference,
                        &mut relation_keys,
                        &mut site_keys,
                    )? {
                        return Err(CodegraphError::InvalidInput(format!(
                            "duplicate {repeated}"
                        )));
                    }
                    blocks::attach_referenced(
                        &transaction,
                        &workspace_text,
                        generation,
                        &reference,
                    )?;
                }
                StagedRow::Full {
                    facts: stored,
                    payload_digest,
                } => {
                    for relation in &stored.relations {
                        if !relation_keys.insert(&relation.key.0) {
                            return Err(CodegraphError::InvalidInput(format!(
                                "duplicate relation_key {}",
                                relation.key.0
                            )));
                        }
                    }
                    for reference in &stored.unresolved_references {
                        if !site_keys.insert(&reference.key.0) {
                            return Err(CodegraphError::InvalidInput(format!(
                                "duplicate site_key {}",
                                reference.key.0
                            )));
                        }
                    }
                    let diagnostic = serde_json::to_string(&stored.diagnostic)
                        .map_err(|error| CodegraphError::InvalidInput(error.to_string()))?;
                    let file = blocks::FileFacts {
                        path: path.as_str(),
                        source_digest: &source_digest,
                        file_id: Uuid::new_v5(
                            &super::FILE_NS,
                            format!("{repository}\0{path}").as_bytes(),
                        ),
                        diagnostic: Some(diagnostic),
                        nodes: &stored.nodes,
                        relations: &stored.relations,
                        unresolved: &stored.unresolved_references,
                    };
                    let resolve = |key: &FactKey| -> Result<Uuid> {
                        node_ids
                            .get(&key.0)
                            .copied()
                            .ok_or_else(|| CodegraphError::MissingNode(key.0.clone()))
                    };
                    let digest_of = |evidence_path: &str| -> Result<String> {
                        if evidence_path == path {
                            Ok(source_digest.clone())
                        } else {
                            Err(CodegraphError::InvalidInput(format!(
                                "evidence for {path} names another file: {evidence_path}"
                            )))
                        }
                    };
                    let built = blocks::build_file_blocks(repository, &file, &resolve, &digest_of)?;
                    let stored_blocks = blocks::store_file_blocks(
                        &transaction,
                        &workspace_text,
                        generation,
                        built,
                    )?;
                    blocks::record_payload_ref(
                        &transaction,
                        &blocks::PayloadRef {
                            path: path.clone(),
                            source_digest: source_digest.clone(),
                            payload_digest,
                            blocks: stored_blocks,
                        },
                    )?;
                }
            }
            // The file's facts now live in shared blocks. Releasing the staged
            // copy inside the publish transaction lets the pages be reused, so
            // a large workspace never holds both copies at once; a failed or
            // faulted publish rolls the delete back with everything else.
            transaction.execute(
                "DELETE FROM cg_stage_files WHERE workspace_id=?1 AND path=?2",
                params![workspace_text, path],
            )?;
        }
        transaction.execute(
            "INSERT INTO cg_snapshot_completeness
             (workspace_id,generation,parsed_files,degraded_files,total_files)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                run.workspace_id.to_string(),
                generation,
                parsed,
                owner_paths.len() - parsed,
                owner_paths.len()
            ],
        )?;
        transaction.execute(
            "UPDATE cg_snapshots SET ready=1 WHERE workspace_id=?1 AND generation=?2",
            params![run.workspace_id.to_string(), generation],
        )?;
        if fault == PublishFault::BeforeHeadFlip {
            return Err(CodegraphError::InjectedPublishFailure);
        }
        transaction.execute(
            "INSERT INTO cg_workspace_heads(workspace_id,generation) VALUES (?1,?2)
             ON CONFLICT(workspace_id) DO UPDATE SET generation=excluded.generation",
            params![run.workspace_id.to_string(), generation],
        )?;
        transaction.execute(
            "DELETE FROM cg_stage_files WHERE workspace_id=?1",
            [run.workspace_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM cg_stage_inventory WHERE workspace_id=?1",
            [run.workspace_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM cg_stage_runs WHERE workspace_id=?1",
            [run.workspace_id.to_string()],
        )?;
        let ready = super::read_snapshot(&transaction, run.workspace_id, generation)?;
        transaction.commit()?;
        Ok(ready)
    }

    /// Read the completeness rollup of the current staged-ready generation.
    /// S0 generations report all files as parsed by their strict bundle contract.
    ///
    /// # Errors
    /// Returns an error if no ready generation exists or `SQLite` fails.
    pub fn current_completeness(&self, workspace_id: Uuid) -> Result<StageCompleteness> {
        let ready = self.current_ready(workspace_id)?;
        let rollup: Option<(i64, i64, i64)> = self.connection.query_row(
            "SELECT total_files,parsed_files,degraded_files FROM cg_snapshot_completeness WHERE workspace_id=?1 AND generation=?2",
            params![workspace_id.to_string(), ready.generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let (total_files, parsed_files, degraded_files) = if let Some(rollup) = rollup {
            rollup
        } else {
            let count = self.connection.query_row(
                "SELECT count(*) FROM cg_manifest m JOIN cg_blocks b ON b.block_id=m.block_id
                 WHERE m.workspace_id=?1 AND m.generation=?2 AND b.kind='S'",
                params![workspace_id.to_string(), ready.generation],
                |row| row.get(0),
            )?;
            (count, count, 0)
        };
        Ok(StageCompleteness {
            total_files: checked_count(total_files)?,
            parsed_files: checked_count(parsed_files)?,
            degraded_files: checked_count(degraded_files)?,
        })
    }

    /// Read the typed diagnostic for a file in the current ready generation.
    ///
    /// # Errors
    /// Returns an error if the file is not ready or its diagnostic is invalid.
    pub fn current_file_diagnostic(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<FileDiagnostic> {
        validate_path(path)?;
        let ready = self.current_ready(workspace_id)?;
        // A file block always exists for a published file; its diagnostic is
        // absent only for S0 bundles, whose files are all parsed by contract.
        let stored: Option<Option<String>> = self
            .connection
            .query_row(
                &format!(
                    "SELECT b.diagnostic FROM cg_blocks b WHERE b.kind='S' AND b.path=?3 AND {}",
                    blocks::scope_sql("b")
                ),
                params![workspace_id.to_string(), ready.generation, path],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(Some(diagnostic)) => serde_json::from_str(&diagnostic)
                .map_err(|error| CodegraphError::InvalidInput(error.to_string())),
            Some(None) => Ok(FileDiagnostic::Parsed),
            None => Err(CodegraphError::InvalidInput(format!(
                "file {path:?} is not in current ready snapshot"
            ))),
        }
    }
}

fn retryable_writer_conflict(error: CodegraphError) -> CodegraphError {
    if let CodegraphError::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = &error
        && matches!(
            code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    {
        return CodegraphError::RetryableWriterConflict;
    }
    error
}

fn verify_stage_counters(
    transaction: &Transaction<'_>,
    run: &StageRun,
    paths: usize,
) -> Result<()> {
    let recorded: (i64, i64, i64, i64) = transaction.query_row(
        "SELECT file_count,source_bytes,fact_count,payload_bytes FROM cg_stage_runs WHERE workspace_id=?1 AND run_id=?2",
        params![run.workspace_id.to_string(), run.run_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let actual: (i64, i64, i64, i64) = transaction.query_row(
        "SELECT count(*),coalesce(sum(source_bytes),0),coalesce(sum(fact_count),0),coalesce(sum(length(payload)),0)
         FROM cg_stage_files WHERE workspace_id=?1",
        [run.workspace_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if recorded != actual || checked_count(recorded.0)? != paths {
        return Err(CodegraphError::InvalidInput(
            "stage counters disagree with staged file rows".into(),
        ));
    }
    limit("files", checked_count(recorded.0)?, MAX_STAGED_FILES)?;
    limit(
        "source bytes",
        checked_count(recorded.1)?,
        MAX_STAGED_SOURCE_BYTES,
    )?;
    limit(
        "total payload bytes",
        checked_count(recorded.3)?,
        MAX_STAGED_TOTAL_PAYLOAD_BYTES,
    )?;
    Ok(())
}

fn stage_extraction(transaction: &Transaction<'_>, run: &StageRun) -> Result<ExtractionContract> {
    transaction.query_row(
        "SELECT extraction_mode,extractor_name,extractor_version FROM cg_stage_runs WHERE workspace_id=?1 AND run_id=?2",
        params![run.workspace_id.to_string(), run.run_id.to_string()],
        |row| Ok(ExtractionContract { mode: parse_extraction_mode(&row.get::<_, String>(0)?)?,
            extractor: super::ExtractorIdentity { name: row.get(1)?, version: row.get(2)? } }),
    ).map_err(Into::into)
}

fn staged_digests(
    transaction: &Transaction<'_>,
    run: &StageRun,
    project: Uuid,
    repository: Uuid,
    owner_paths: &[String],
    source_digest_domain: &[u8],
) -> Result<(String, String)> {
    let manifest: (String, String, String, String) = transaction.query_row(
        "SELECT grammar_version,rule_version,normalization_version,config_digest FROM cg_stage_runs WHERE workspace_id=?1 AND run_id=?2",
        params![run.workspace_id.to_string(), run.run_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let extraction = stage_extraction(transaction, run)?;
    let mut hasher = blake3::Hasher::new();
    for part in [
        b"rsi-codegraph-staged-v2".as_slice(),
        source_digest_domain,
        project.as_bytes(),
        repository.as_bytes(),
        run.workspace_id.as_bytes(),
        extraction.mode.as_str().as_bytes(),
        extraction.extractor.name.as_bytes(),
        extraction.extractor.version.as_bytes(),
        manifest.0.as_bytes(),
        manifest.1.as_bytes(),
        manifest.2.as_bytes(),
        manifest.3.as_bytes(),
    ] {
        hash_field(&mut hasher, part);
    }
    hasher.update(&(owner_paths.len() as u64).to_be_bytes());
    for path in owner_paths {
        let (source_digest, payload): (String, Vec<u8>) = transaction.query_row(
            "SELECT source_digest,payload FROM cg_stage_files WHERE workspace_id=?1 AND path=?2",
            params![run.workspace_id.to_string(), path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        limit(
            "file payload bytes",
            payload.len(),
            MAX_STAGED_PAYLOAD_BYTES,
        )?;
        hash_field(&mut hasher, path.as_bytes());
        hash_field(&mut hasher, source_digest.as_bytes());
        // The digest of the canonical facts stands for them, so a file staged
        // as a reference to its stored blocks hashes like one staged in full.
        hash_field(&mut hasher, row_payload_digest(path, &payload)?.as_bytes());
    }
    let base = hasher.finalize();
    let snapshot = blake3::derive_key("rsi-codegraph-staged-snapshot-v2", base.as_bytes());
    let graph = blake3::derive_key("rsi-codegraph-staged-graph-v2", base.as_bytes());
    Ok((
        blake3::Hash::from_bytes(snapshot).to_hex().to_string(),
        blake3::Hash::from_bytes(graph).to_hex().to_string(),
    ))
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Fixtures fail at their exact construction point.
mod tests {
    use super::*;
    use crate::ExtractionMode;

    #[test]
    fn every_staged_resource_ceiling_reports_overflow() {
        for maximum in [
            MAX_STAGED_FILES,
            MAX_STAGED_FILE_BYTES,
            MAX_STAGED_SOURCE_BYTES,
            MAX_STAGED_FACTS_PER_FILE,
            MAX_STAGED_PAYLOAD_BYTES,
            MAX_STAGED_BUFFER_BYTES,
            MAX_STAGED_TOTAL_PAYLOAD_BYTES,
        ] {
            assert!(matches!(
                limit("ceiling", maximum.saturating_add(1), maximum),
                Err(CodegraphError::StageCeilingExceeded { .. })
            ));
        }
    }

    #[test]
    fn source_digest_algorithm_marker_changes_both_digest_domains() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            CodegraphStore::open(directory.path().join("graph.sqlite"), Uuid::nil()).unwrap();
        let scope = store.scope(super::super::WorkspaceInstanceKey::Primary);
        let file = SourceFile {
            relative_path: "src/a.rs".into(),
            bytes: b"alpha".to_vec(),
        };
        let owner = SourceVersion {
            relative_path: file.relative_path.clone(),
            source_digest: blake3::hash(&file.bytes).to_hex().to_string(),
        };
        let manifest = StagedExtraction {
            extraction: ExtractionContract {
                mode: ExtractionMode::ExtractedV1_0,
                extractor: super::super::ExtractorIdentity {
                    name: "fixture".into(),
                    version: "1".into(),
                },
            },
            grammar_version: "grammar-1".into(),
            rule_version: "rules-1".into(),
            normalization_version: "normalization-1".into(),
            config_digest: "config-1".into(),
        };
        let run = store.begin_staged(&scope, &manifest, &[owner]).unwrap();
        store
            .stage_file(
                &run,
                &PerFileFacts {
                    file,
                    nodes: vec![],
                    relations: vec![],
                    unresolved_references: vec![],
                    diagnostic: FileDiagnostic::Parsed,
                    scope: crate::scope::RustScope::default(),
                },
            )
            .unwrap();
        let transaction = store.connection.transaction().unwrap();
        let paths = paths(&transaction, run.workspace_id).unwrap();
        let current = staged_digests(
            &transaction,
            &run,
            Uuid::nil(),
            CodegraphStore::repository_id(Uuid::nil()),
            &paths,
            SOURCE_DIGEST_DOMAIN,
        )
        .unwrap();
        let changed = staged_digests(
            &transaction,
            &run,
            Uuid::nil(),
            CodegraphStore::repository_id(Uuid::nil()),
            &paths,
            b"other-digest-v1",
        )
        .unwrap();
        assert_ne!(current.0, changed.0);
        assert_ne!(current.1, changed.1);
        drop(transaction);
        let ready = store.publish_staged(&run, PublishFault::None).unwrap();
        assert_eq!((ready.snapshot_digest, ready.graph_digest), current);
    }
}
