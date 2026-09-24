//! Exact, evidence-bearing structural facts and bounded queries.
//!
//! Extraction intentionally lives behind [`SourceFactBundle`]. S0 callers
//! supply facts; parsers and filesystem discovery belong to a later layer.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::{Uuid, uuid};

const SCHEMA_VERSION: i64 = 6;

pub mod cargo_metadata;
pub mod extract;
pub mod invalidate;
pub mod lifecycle;
pub mod query;
pub mod resolve;
pub mod staged;
pub use staged::{
    FileDiagnostic, PerFileFacts, SourceVersion, StageCompleteness, StageRun, StagedExtraction,
};
const REPOSITORY_NS: Uuid = uuid!("ef4a5ef3-9a44-5fb8-936c-6b636e0957d5");
const WORKSPACE_NS: Uuid = uuid!("7b7c1853-a9ea-5344-b8c6-7c547807bfad");
const NODE_NS: Uuid = uuid!("db0bcaea-b967-57b7-9803-81130f258752");
const RELATION_NS: Uuid = uuid!("34bed330-f70b-5232-9431-82262e5833df");
const FILE_NS: Uuid = uuid!("c707c51b-25ec-5ba4-92f9-ccf42b187194");

pub const STRICT_EXTRACTION_MODE: &str = "EXTRACTED/1.0";
pub const MAX_FILES: usize = 128;
pub const MAX_FACTS: usize = 10_000;
pub const MAX_FILE_BYTES: usize = 1_048_576;
pub const MAX_TOTAL_SOURCE_BYTES: usize = 16_777_216;
pub const MAX_PATH_BYTES: usize = 1_024;
pub const MAX_NAME_BYTES: usize = 512;
pub const MAX_EVIDENCE_LABEL_BYTES: usize = 1_024;
pub const MAX_QUERY_LIMIT: usize = 128;
pub const MAX_EXPLAIN_RELATIONS: usize = 256;
pub const MAX_EXPLAIN_UNRESOLVED: usize = 256;
pub const MAX_EVIDENCE_PER_FACT: usize = 64;
pub const MAX_EVIDENCE_SPAN_BYTES: usize = 4_096;
pub const MAX_UNRESOLVED_TARGET_BYTES: usize = 1_024;
pub const MAX_QUERY_OUTPUT_BYTES: usize = 524_288;
pub const MAX_EXTRACTOR_NAME_BYTES: usize = 128;
pub const MAX_EXTRACTOR_VERSION_BYTES: usize = 64;
pub const MAX_LANGUAGE_BYTES: usize = 64;
pub const MAX_SITE_ANCHOR_BYTES: usize = 1_024;

#[derive(Debug, Error)]
pub enum CodegraphError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("requested limit {requested} exceeds hard limit {maximum}")]
    LimitExceeded { requested: usize, maximum: usize },
    #[error("node key is not present in this source-fact bundle: {0}")]
    MissingNode(String),
    #[error("current ready snapshot does not exist")]
    NoReadySnapshot,
    #[error("injected failure before ready-head publication")]
    InjectedPublishFailure,
    #[error(
        "retryable SQLite writer conflict; retry the whole publication within the original run deadline"
    )]
    RetryableWriterConflict,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, CodegraphError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub relative_path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    Workspace,
    Crate,
    File,
    Module,
    Function,
    Method,
    Struct,
    Trait,
    Enum,
    EnumVariant,
    TypeAlias,
    Const,
    Static,
    Impl,
    Test,
    CargoTarget,
    MarkdownDocument,
    MarkdownHeading,
    StageContract,
    ResearchFinding,
    PlanItem,
    DecisionRecord,
    RationaleMarker,
}

impl NodeKind {
    pub const ALL: &'static [Self] = &[
        Self::Workspace,
        Self::Crate,
        Self::File,
        Self::Module,
        Self::Function,
        Self::Method,
        Self::Struct,
        Self::Trait,
        Self::Enum,
        Self::EnumVariant,
        Self::TypeAlias,
        Self::Const,
        Self::Static,
        Self::Impl,
        Self::Test,
        Self::CargoTarget,
        Self::MarkdownDocument,
        Self::MarkdownHeading,
        Self::StageContract,
        Self::ResearchFinding,
        Self::PlanItem,
        Self::DecisionRecord,
        Self::RationaleMarker,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Crate => "Crate",
            Self::File => "File",
            Self::Module => "Module",
            Self::Function => "Function",
            Self::Method => "Method",
            Self::Struct => "Struct",
            Self::Trait => "Trait",
            Self::Enum => "Enum",
            Self::EnumVariant => "EnumVariant",
            Self::TypeAlias => "TypeAlias",
            Self::Const => "Const",
            Self::Static => "Static",
            Self::Impl => "Impl",
            Self::Test => "Test",
            Self::CargoTarget => "CargoTarget",
            Self::MarkdownDocument => "MarkdownDocument",
            Self::MarkdownHeading => "MarkdownHeading",
            Self::StageContract => "StageContract",
            Self::ResearchFinding => "ResearchFinding",
            Self::PlanItem => "PlanItem",
            Self::DecisionRecord => "DecisionRecord",
            Self::RationaleMarker => "RationaleMarker",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FactKey(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub language: String,
    pub qualified_name: String,
    pub disambiguator: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FactProvenance {
    Extracted,
    Inferred,
    Ambiguous,
}

impl FactProvenance {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Extracted => "Extracted",
            Self::Inferred => "Inferred",
            Self::Ambiguous => "Ambiguous",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExtractionMode {
    ExtractedV1_0,
    Exploratory,
}

impl ExtractionMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ExtractedV1_0 => STRICT_EXTRACTION_MODE,
            Self::Exploratory => "EXPLORATORY/1.0",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractorIdentity {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionContract {
    pub mode: ExtractionMode,
    pub extractor: ExtractorIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFact {
    pub key: FactKey,
    pub identity: NodeIdentity,
    pub kind: NodeKind,
    pub name: String,
    pub span: SourceSpan,
    pub provenance: FactProvenance,
    pub evidence: Vec<EvidenceFact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationKind {
    Contains,
    Declares,
    Calls,
    UsesType,
    Defines,
    Implements,
    HasMethod,
    Imports,
    DependsOn,
    DevDependsOn,
    BuildDependsOn,
    Tests,
    Documents,
    ReferencesDoc,
    ReferencesFinding,
    SatisfiesFinding,
    Supersedes,
}

impl RelationKind {
    pub const ALL: &'static [Self] = &[
        Self::Contains,
        Self::Declares,
        Self::Calls,
        Self::UsesType,
        Self::Defines,
        Self::Implements,
        Self::HasMethod,
        Self::Imports,
        Self::DependsOn,
        Self::DevDependsOn,
        Self::BuildDependsOn,
        Self::Tests,
        Self::Documents,
        Self::ReferencesDoc,
        Self::ReferencesFinding,
        Self::SatisfiesFinding,
        Self::Supersedes,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "Contains",
            Self::Declares => "Declares",
            Self::Calls => "Calls",
            Self::UsesType => "UsesType",
            Self::Defines => "Defines",
            Self::Implements => "Implements",
            Self::HasMethod => "HasMethod",
            Self::Imports => "Imports",
            Self::DependsOn => "DependsOn",
            Self::DevDependsOn => "DevDependsOn",
            Self::BuildDependsOn => "BuildDependsOn",
            Self::Tests => "Tests",
            Self::Documents => "Documents",
            Self::ReferencesDoc => "ReferencesDoc",
            Self::ReferencesFinding => "ReferencesFinding",
            Self::SatisfiesFinding => "SatisfiesFinding",
            Self::Supersedes => "Supersedes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceFact {
    pub label: String,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationFact {
    pub key: FactKey,
    pub owner_file: String,
    /// Versioned structural site key (`vN:path`), stable across line movement.
    pub site_anchor: String,
    pub kind: RelationKind,
    pub source: FactKey,
    pub target: FactKey,
    pub provenance: FactProvenance,
    pub evidence: Vec<EvidenceFact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnresolvedReferenceKind {
    Call,
    Type,
    ImplTrait,
    Import,
    Finding,
    Document,
    Other,
}

impl UnresolvedReferenceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Call => "Call",
            Self::Type => "Type",
            Self::ImplTrait => "ImplTrait",
            Self::Import => "Import",
            Self::Finding => "Finding",
            Self::Document => "Document",
            Self::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedReferenceFact {
    pub key: FactKey,
    pub owner: FactKey,
    pub kind: UnresolvedReferenceKind,
    pub raw_target: String,
    pub span: SourceSpan,
    pub provenance: FactProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFactBundle {
    pub extraction: ExtractionContract,
    pub files: Vec<SourceFile>,
    pub nodes: Vec<NodeFact>,
    pub relations: Vec<RelationFact>,
    pub unresolved_references: Vec<UnresolvedReferenceFact>,
}

/// Project-owned workspace identity; fields stay private so callers cannot
/// forge a workspace UUID detached from its repository and typed instance key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceScope {
    project_id: Uuid,
    repository_id: Uuid,
    workspace_id: Uuid,
    instance_key: WorkspaceInstanceKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceInstanceKey {
    Primary,
    GitWorktree(Uuid),
    RsiSandbox(Uuid),
    DetachedRootHash([u8; 32]),
}

impl WorkspaceInstanceKey {
    fn canonical_key(&self) -> String {
        match self {
            Self::Primary => "primary".to_owned(),
            Self::GitWorktree(id) => format!("git-worktree:{id}"),
            Self::RsiSandbox(id) => format!("rsi-sandbox:{id}"),
            Self::DetachedRootHash(hash) => {
                format!("detached:{}", blake3::Hash::from_bytes(*hash).to_hex())
            }
        }
    }
}

impl WorkspaceScope {
    #[must_use]
    pub const fn project_id(&self) -> Uuid {
        self.project_id
    }

    #[must_use]
    pub const fn repository_id(&self) -> Uuid {
        self.repository_id
    }

    #[must_use]
    pub const fn workspace_id(&self) -> Uuid {
        self.workspace_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadySnapshot {
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub workspace_id: Uuid,
    pub generation: i64,
    pub snapshot_digest: String,
    pub graph_digest: String,
    pub extraction: ExtractionContract,
}

/// An exact search result bound to one ready snapshot. `complete` is false when
/// another matching row exists beyond the requested bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub snapshot: ReadySnapshot,
    pub requested_limit: usize,
    pub effective_limit: usize,
    pub complete: bool,
    pub nodes: Vec<NodeView>,
}

impl std::ops::Deref for SearchResult {
    type Target = [NodeView];

    fn deref(&self) -> &Self::Target {
        &self.nodes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishFault {
    None,
    BeforeHeadFlip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceView {
    pub label: String,
    pub span: SourceSpan,
    pub source_digest: String,
    pub evidence_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeView {
    pub id: Uuid,
    pub kind: NodeKind,
    pub name: String,
    pub span: SourceSpan,
    pub provenance: FactProvenance,
    pub evidence: Vec<EvidenceView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationView {
    pub id: Uuid,
    pub kind: RelationKind,
    pub source: Uuid,
    pub target: Uuid,
    pub provenance: FactProvenance,
    pub evidence: Vec<EvidenceView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainView {
    pub snapshot: ReadySnapshot,
    pub node: NodeView,
    pub ownership: Vec<RelationView>,
    pub directed_relations: Vec<RelationView>,
    pub unresolved_references: Vec<UnresolvedReferenceView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedReferenceView {
    pub kind: UnresolvedReferenceKind,
    pub owner: Uuid,
    pub raw_target: String,
    pub span: SourceSpan,
    pub source_digest: String,
    pub reference_digest: String,
}

/// One dedicated structural database per logical project, with workspace heads.
pub struct CodegraphStore {
    connection: Connection,
    project_id: Uuid,
}

impl CodegraphStore {
    /// Open or initialize the structural database bound to `project_id`.
    ///
    /// # Errors
    /// Returns an error for unsupported schema versions, a different stored
    /// project identity, migration failures, or `SQLite` I/O failures.
    #[allow(clippy::too_many_lines)] // Inline migrations stay adjacent to the open/version gate.
    pub fn open(path: impl AsRef<Path>, project_id: Uuid) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(CodegraphError::InvalidInput(format!(
                "database schema {version} is newer than supported schema {SCHEMA_VERSION}"
            )));
        }
        if version < 1 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "CREATE TABLE cg_store_meta (
                    project_id TEXT PRIMARY KEY
                );
                CREATE TABLE cg_snapshots (
                    project_id TEXT NOT NULL,
                    repository_id TEXT NOT NULL,
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    snapshot_digest TEXT NOT NULL,
                    graph_digest TEXT NOT NULL,
                    ready INTEGER NOT NULL CHECK (ready IN (0,1)),
                    PRIMARY KEY (workspace_id, generation)
                );
                CREATE TABLE cg_workspace_heads (
                    workspace_id TEXT PRIMARY KEY,
                    generation INTEGER NOT NULL,
                    FOREIGN KEY (workspace_id, generation)
                        REFERENCES cg_snapshots(workspace_id, generation)
                );
                CREATE TABLE cg_files (
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    path TEXT NOT NULL,
                    file_id TEXT NOT NULL,
                    bytes BLOB NOT NULL,
                    source_digest TEXT NOT NULL,
                    PRIMARY KEY (workspace_id, generation, path),
                    FOREIGN KEY (workspace_id, generation)
                        REFERENCES cg_snapshots(workspace_id, generation)
                );
                CREATE TABLE cg_nodes (
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    node_id TEXT NOT NULL,
                    node_key TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    name TEXT NOT NULL,
                    path TEXT NOT NULL,
                    start_byte INTEGER NOT NULL,
                    end_byte INTEGER NOT NULL,
                    start_line INTEGER NOT NULL,
                    start_column INTEGER NOT NULL,
                    end_line INTEGER NOT NULL,
                    end_column INTEGER NOT NULL,
                    PRIMARY KEY (workspace_id, generation, node_id),
                    UNIQUE (workspace_id, generation, node_key),
                    FOREIGN KEY (workspace_id, generation, path)
                        REFERENCES cg_files(workspace_id, generation, path)
                );
                CREATE INDEX cg_nodes_name ON cg_nodes(workspace_id, generation, name);
                CREATE INDEX cg_nodes_path ON cg_nodes(workspace_id, generation, path);
                CREATE TABLE cg_relations (
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    relation_id TEXT NOT NULL,
                    relation_key TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    source_id TEXT NOT NULL,
                    target_id TEXT NOT NULL,
                    PRIMARY KEY (workspace_id, generation, relation_id),
                    UNIQUE (workspace_id, generation, relation_key),
                    FOREIGN KEY (workspace_id, generation, source_id)
                        REFERENCES cg_nodes(workspace_id, generation, node_id),
                    FOREIGN KEY (workspace_id, generation, target_id)
                        REFERENCES cg_nodes(workspace_id, generation, node_id)
                );
                CREATE INDEX cg_relations_endpoints ON cg_relations(workspace_id, generation, source_id, target_id);
                CREATE TABLE cg_evidence (
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    fact_id TEXT NOT NULL,
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
                    PRIMARY KEY (workspace_id, generation, fact_id, ordinal),
                    FOREIGN KEY (workspace_id, generation, path)
                        REFERENCES cg_files(workspace_id, generation, path)
                );
                PRAGMA user_version = 1;",
            )?;
            transaction.execute(
                "INSERT INTO cg_store_meta(project_id) VALUES (?1)",
                [project_id.to_string()],
            )?;
            transaction.commit()?;
        }
        let stored_project: String =
            connection.query_row("SELECT project_id FROM cg_store_meta LIMIT 1", [], |row| {
                row.get(0)
            })?;
        if parse_uuid(&stored_project)? != project_id {
            return Err(CodegraphError::InvalidInput(
                "structural database is already bound to a different project".into(),
            ));
        }
        if version < 2 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "ALTER TABLE cg_snapshots ADD COLUMN extraction_mode TEXT NOT NULL DEFAULT 'EXTRACTED/1.0';
                ALTER TABLE cg_snapshots ADD COLUMN extractor_name TEXT NOT NULL DEFAULT 'legacy-v1-migration';
                ALTER TABLE cg_snapshots ADD COLUMN extractor_version TEXT NOT NULL DEFAULT '1';
                ALTER TABLE cg_nodes ADD COLUMN provenance TEXT NOT NULL DEFAULT 'Extracted'
                    CHECK (provenance IN ('Extracted','Inferred','Ambiguous'));
                ALTER TABLE cg_relations ADD COLUMN provenance TEXT NOT NULL DEFAULT 'Extracted'
                    CHECK (provenance IN ('Extracted','Inferred','Ambiguous'));
                ALTER TABLE cg_evidence ADD COLUMN fact_type TEXT NOT NULL DEFAULT 'relation'
                    CHECK (fact_type IN ('node','relation'));
                CREATE INDEX cg_evidence_fact ON cg_evidence(workspace_id,generation,fact_type,fact_id);
                CREATE TABLE cg_unresolved_references (
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    unresolved_id TEXT NOT NULL,
                    site_key TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    owner_id TEXT NOT NULL,
                    raw_target TEXT NOT NULL,
                    path TEXT NOT NULL,
                    start_byte INTEGER NOT NULL,
                    end_byte INTEGER NOT NULL,
                    start_line INTEGER NOT NULL,
                    start_column INTEGER NOT NULL,
                    end_line INTEGER NOT NULL,
                    end_column INTEGER NOT NULL,
                    source_digest TEXT NOT NULL,
                    reference_digest TEXT NOT NULL,
                    provenance TEXT NOT NULL DEFAULT 'Extracted'
                        CHECK (provenance IN ('Extracted','Inferred','Ambiguous')),
                    PRIMARY KEY (workspace_id,generation,unresolved_id),
                    UNIQUE (workspace_id,generation,site_key),
                    FOREIGN KEY (workspace_id,generation,owner_id)
                        REFERENCES cg_nodes(workspace_id,generation,node_id),
                    FOREIGN KEY (workspace_id,generation,path)
                        REFERENCES cg_files(workspace_id,generation,path)
                );
                CREATE INDEX cg_unresolved_owner ON cg_unresolved_references(workspace_id,generation,owner_id);",
            )?;
            migrate_v1_to_v2(&transaction)?;
            transaction.execute_batch("PRAGMA user_version = 2;")?;
            transaction.commit()?;
        }
        if version < 3 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "CREATE TABLE cg_stage_runs (
                    workspace_id TEXT PRIMARY KEY,
                    run_id TEXT NOT NULL,
                    started_at TEXT NOT NULL,
                    extraction_mode TEXT NOT NULL,
                    extractor_name TEXT NOT NULL,
                    extractor_version TEXT NOT NULL,
                    grammar_version TEXT NOT NULL,
                    rule_version TEXT NOT NULL,
                    normalization_version TEXT NOT NULL,
                    config_digest TEXT NOT NULL,
                    file_count INTEGER NOT NULL DEFAULT 0,
                    source_bytes INTEGER NOT NULL DEFAULT 0,
                    fact_count INTEGER NOT NULL DEFAULT 0,
                    payload_bytes INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE cg_stage_files (
                    workspace_id TEXT NOT NULL,
                    path TEXT NOT NULL,
                    source_digest TEXT NOT NULL,
                    source_bytes INTEGER NOT NULL,
                    fact_count INTEGER NOT NULL,
                    payload BLOB NOT NULL,
                    PRIMARY KEY (workspace_id,path),
                    FOREIGN KEY (workspace_id) REFERENCES cg_stage_runs(workspace_id)
                );
                CREATE TABLE cg_stage_inventory (
                    workspace_id TEXT NOT NULL,
                    path TEXT NOT NULL,
                    source_digest TEXT NOT NULL,
                    PRIMARY KEY (workspace_id,path),
                    FOREIGN KEY (workspace_id) REFERENCES cg_stage_runs(workspace_id)
                );
                CREATE TABLE cg_snapshot_completeness (
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    parsed_files INTEGER NOT NULL,
                    degraded_files INTEGER NOT NULL,
                    total_files INTEGER NOT NULL,
                    PRIMARY KEY (workspace_id,generation),
                    FOREIGN KEY (workspace_id,generation)
                        REFERENCES cg_snapshots(workspace_id,generation)
                );
                CREATE TABLE cg_file_diagnostics (
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    path TEXT NOT NULL,
                    diagnostic TEXT NOT NULL,
                    PRIMARY KEY (workspace_id,generation,path),
                    FOREIGN KEY (workspace_id,generation,path)
                        REFERENCES cg_files(workspace_id,generation,path)
                );
                PRAGMA user_version = 3;",
            )?;
            transaction.commit()?;
        }
        if version < 4 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS cg_fts_nodes USING fts5(
                    name, path,
                    workspace_id UNINDEXED, generation UNINDEXED,
                    node_id UNINDEXED, owner_path UNINDEXED,
                    tokenize='unicode61'
                );
                DELETE FROM cg_fts_nodes;
                INSERT INTO cg_fts_nodes(name,path,workspace_id,generation,node_id,owner_path)
                SELECT n.name,n.path,n.workspace_id,n.generation,n.node_id,n.path
                FROM cg_nodes n JOIN cg_snapshots s
                  ON s.workspace_id=n.workspace_id AND s.generation=n.generation
                WHERE s.ready=1 ORDER BY n.workspace_id,n.generation,n.path,n.start_byte,n.node_id;
                PRAGMA user_version = 4;",
            )?;
            transaction.commit()?;
        }
        if version < 5 {
            // V5 is additive. Released V1--V4 catalog and migration text stay
            // immutable; lifecycle state belongs only to this project DB.
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "CREATE TABLE cg_index_state (
                    workspace_id TEXT PRIMARY KEY,
                    phase TEXT NOT NULL CHECK (phase IN
                        ('Queued','Building','Ready','Stale','Degraded','Failed','Recovering')),
                    run_id TEXT,
                    source_digest TEXT,
                    last_attempt_at TEXT,
                    last_success_at TEXT,
                    last_duration_ms INTEGER CHECK (last_duration_ms IS NULL OR last_duration_ms >= 0),
                    files_discovered INTEGER NOT NULL DEFAULT 0 CHECK (files_discovered >= 0),
                    bytes_discovered INTEGER NOT NULL DEFAULT 0 CHECK (bytes_discovered >= 0),
                    changed_paths INTEGER NOT NULL DEFAULT 0 CHECK (changed_paths >= 0),
                    staged_files INTEGER NOT NULL DEFAULT 0 CHECK (staged_files >= 0),
                    overflow_count INTEGER NOT NULL DEFAULT 0 CHECK (overflow_count >= 0),
                    pending_rescan INTEGER NOT NULL DEFAULT 0 CHECK (pending_rescan IN (0,1)),
                    last_error TEXT,
                    recovery_action TEXT,
                    extractor_version TEXT,
                    grammar_version TEXT,
                    rule_version TEXT,
                    normalization_version TEXT,
                    config_digest TEXT,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE cg_index_runs (
                    run_id TEXT PRIMARY KEY,
                    workspace_id TEXT NOT NULL,
                    phase TEXT NOT NULL CHECK (phase IN ('Building','Ready','Failed','Interrupted')),
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    source_digest TEXT NOT NULL,
                    error TEXT
                );
                CREATE INDEX cg_index_runs_workspace_started
                    ON cg_index_runs(workspace_id,started_at DESC);
                CREATE TABLE cg_index_manifest (
                    workspace_id TEXT NOT NULL,
                    path TEXT NOT NULL,
                    source_digest TEXT NOT NULL,
                    PRIMARY KEY (workspace_id,path)
                );
                CREATE TABLE cg_generation_detail (
                    workspace_id TEXT NOT NULL,
                    generation INTEGER NOT NULL,
                    published_at TEXT,
                    detailed_bytes INTEGER NOT NULL DEFAULT 0 CHECK (detailed_bytes >= 0),
                    PRIMARY KEY (workspace_id,generation),
                    FOREIGN KEY (workspace_id,generation)
                        REFERENCES cg_snapshots(workspace_id,generation)
                );
                CREATE TABLE cg_retained_nodes (
                    workspace_id TEXT NOT NULL,
                    node_id TEXT NOT NULL,
                    node_key TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    name TEXT NOT NULL,
                    last_detailed_generation INTEGER NOT NULL,
                    tombstoned INTEGER NOT NULL CHECK (tombstoned IN (0,1)),
                    PRIMARY KEY (workspace_id,node_id)
                );
                CREATE TABLE cg_retained_relations (
                    workspace_id TEXT NOT NULL,
                    relation_id TEXT NOT NULL,
                    relation_key TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    source_id TEXT NOT NULL,
                    target_id TEXT NOT NULL,
                    last_detailed_generation INTEGER NOT NULL,
                    tombstoned INTEGER NOT NULL CHECK (tombstoned IN (0,1)),
                    PRIMARY KEY (workspace_id,relation_id)
                );
                INSERT INTO cg_index_state(workspace_id,phase,updated_at)
                SELECT workspace_id,'Ready',strftime('%Y-%m-%dT%H:%M:%fZ','now')
                FROM cg_workspace_heads;
                INSERT INTO cg_generation_detail(workspace_id,generation,published_at,detailed_bytes)
                SELECT workspace_id,generation,NULL,0 FROM cg_snapshots WHERE ready=1;
                PRAGMA user_version = 5;",
            )?;
            transaction.commit()?;
        }
        if version < 6 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "ALTER TABLE cg_index_state ADD COLUMN files_hashed INTEGER NOT NULL DEFAULT 0 CHECK (files_hashed >= 0);
                 ALTER TABLE cg_index_state ADD COLUMN files_reused INTEGER NOT NULL DEFAULT 0 CHECK (files_reused >= 0);
                 ALTER TABLE cg_index_state ADD COLUMN files_extracted INTEGER NOT NULL DEFAULT 0 CHECK (files_extracted >= 0);
                 ALTER TABLE cg_index_state ADD COLUMN rescan_reason TEXT;
                 PRAGMA user_version = 6;",
            )?;
            transaction.commit()?;
        }
        Ok(Self {
            connection,
            project_id,
        })
    }

    #[must_use]
    pub fn repository_id(project_id: Uuid) -> Uuid {
        Uuid::new_v5(&REPOSITORY_NS, format!("{project_id}\0primary").as_bytes())
    }

    /// Derive a stable workspace identity within the project's primary repository.
    ///
    /// # Errors
    /// Returns an error when the instance key is empty, too long, or contains NUL.
    pub fn workspace_id(project_id: Uuid, instance_key: &str) -> Result<Uuid> {
        if instance_key.is_empty()
            || instance_key.len() > MAX_PATH_BYTES
            || instance_key.contains('\0')
        {
            return Err(CodegraphError::InvalidInput(
                "workspace instance key is empty or contains NUL".into(),
            ));
        }
        Ok(derive_workspace_id(project_id, instance_key))
    }

    /// Bind a typed workspace instance to this store's project and repository.
    #[must_use]
    pub fn scope(&self, instance_key: WorkspaceInstanceKey) -> WorkspaceScope {
        let workspace_id = derive_workspace_id(self.project_id, &instance_key.canonical_key());
        WorkspaceScope {
            project_id: self.project_id,
            repository_id: Self::repository_id(self.project_id),
            workspace_id,
            instance_key,
        }
    }

    /// Validate and atomically publish one complete source-fact bundle.
    ///
    /// # Errors
    /// Returns an error for invalid facts, limit violations, or `SQLite` failure;
    /// failed publication leaves the current ready generation intact.
    pub fn publish(
        &mut self,
        scope: &WorkspaceScope,
        bundle: &SourceFactBundle,
    ) -> Result<ReadySnapshot> {
        self.publish_with_fault(scope, bundle, PublishFault::None)
    }

    /// Publish with an explicit fault point used to verify rollback behavior.
    ///
    /// # Errors
    /// Returns an error for invalid facts, `SQLite` failure, or the requested
    /// injected pre-head-flip fault.
    pub fn publish_with_fault(
        &mut self,
        scope: &WorkspaceScope,
        bundle: &SourceFactBundle,
        fault: PublishFault,
    ) -> Result<ReadySnapshot> {
        let project_id = self.project_id;
        let repository_id = Self::repository_id(project_id);
        let workspace_id = Self::workspace_id(project_id, &scope.instance_key.canonical_key())?;
        if scope.project_id != project_id
            || scope.repository_id != repository_id
            || scope.workspace_id != workspace_id
        {
            return Err(CodegraphError::InvalidInput(
                "workspace scope is not bound to this project and repository".into(),
            ));
        }
        validate_bundle(bundle)?;
        let snapshot_digest = bundle_digest(bundle, project_id, repository_id, workspace_id);
        let graph_digest = graph_digest(bundle, project_id, repository_id, workspace_id);
        let transaction = self.connection.transaction()?;
        let head: Option<i64> = transaction
            .query_row(
                "SELECT generation FROM cg_workspace_heads WHERE workspace_id=?1",
                [workspace_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(head_generation) = head {
            let existing: Option<i64> = transaction.query_row(
                "SELECT generation FROM cg_snapshots WHERE workspace_id=?1 AND generation=?2 AND snapshot_digest=?3 AND ready=1",
                params![workspace_id.to_string(), head_generation, snapshot_digest],
                |row| row.get(0),
            ).optional()?;
            if let Some(generation) = existing {
                let snapshot = read_snapshot(&transaction, workspace_id, generation)?;
                transaction.commit()?;
                return Ok(snapshot);
            }
        }
        let generation = head.unwrap_or(0) + 1;
        transaction.execute(
            "INSERT INTO cg_snapshots(project_id,repository_id,workspace_id,generation,snapshot_digest,graph_digest,ready,extraction_mode,extractor_name,extractor_version)
             VALUES (?1,?2,?3,?4,?5,?6,0,?7,?8,?9)",
            params![project_id.to_string(), repository_id.to_string(), workspace_id.to_string(), generation, snapshot_digest, graph_digest, bundle.extraction.mode.as_str(), bundle.extraction.extractor.name, bundle.extraction.extractor.version],
        )?;
        insert_bundle(&transaction, workspace_id, generation, bundle)?;
        transaction.execute(
            "UPDATE cg_snapshots SET ready=1 WHERE workspace_id=?1 AND generation=?2",
            params![workspace_id.to_string(), generation],
        )?;
        if fault == PublishFault::BeforeHeadFlip {
            return Err(CodegraphError::InjectedPublishFailure);
        }
        transaction.execute(
            "INSERT INTO cg_workspace_heads(workspace_id,generation) VALUES (?1,?2)
             ON CONFLICT(workspace_id) DO UPDATE SET generation=excluded.generation",
            params![workspace_id.to_string(), generation],
        )?;
        transaction.commit()?;
        Ok(ReadySnapshot {
            project_id,
            repository_id,
            workspace_id,
            generation,
            snapshot_digest,
            graph_digest,
            extraction: bundle.extraction.clone(),
        })
    }

    /// Return the current ready snapshot for a workspace.
    ///
    /// # Errors
    /// Returns [`CodegraphError::NoReadySnapshot`] if the workspace has no
    /// published generation, or a `SQLite` error if the store cannot be read.
    pub fn current_ready(&self, workspace_id: Uuid) -> Result<ReadySnapshot> {
        let generation: i64 = self
            .connection
            .query_row(
                "SELECT generation FROM cg_workspace_heads WHERE workspace_id=?1",
                [workspace_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(CodegraphError::NoReadySnapshot)?;
        read_snapshot(&self.connection, workspace_id, generation)
    }

    /// List retained ready snapshots newest first. `before_generation` is an
    /// exclusive keyset fence; this never scans unready staging rows.
    pub fn ready_snapshots_before(
        &self,
        workspace_id: Uuid,
        before_generation: Option<i64>,
        limit: usize,
    ) -> Result<Vec<ReadySnapshot>> {
        if limit == 0 || limit > 64 || before_generation.is_some_and(|value| value <= 0) {
            return Err(CodegraphError::InvalidInput(
                "invalid ready snapshot page bound".into(),
            ));
        }
        let mut statement = self.connection.prepare(
            "SELECT generation FROM cg_snapshots
             WHERE workspace_id=?1 AND ready=1
               AND (?2 IS NULL OR generation<?2)
             ORDER BY generation DESC LIMIT ?3",
        )?;
        let generations = statement
            .query_map(
                params![workspace_id.to_string(), before_generation, limit as i64],
                |row| row.get::<_, i64>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        generations
            .into_iter()
            .map(|generation| {
                let snapshot = read_snapshot(&self.connection, workspace_id, generation)?;
                if snapshot.project_id != self.project_id
                    || snapshot.repository_id != Self::repository_id(self.project_id)
                {
                    return Err(CodegraphError::InvalidInput(
                        "retained snapshot scope mismatch".into(),
                    ));
                }
                Ok(snapshot)
            })
            .collect()
    }

    /// Resolve a normalized path to its repository-stable file identity.
    ///
    /// # Errors
    /// Returns an error for an invalid path, missing ready snapshot, or file.
    pub fn file_id(&self, workspace_id: Uuid, path: &str) -> Result<Uuid> {
        validate_path(path)?;
        let snapshot = self.current_ready(workspace_id)?;
        self.connection
            .query_row(
                "SELECT file_id FROM cg_files WHERE workspace_id=?1 AND generation=?2 AND path=?3",
                params![workspace_id.to_string(), snapshot.generation, path],
                |row| {
                    let id: String = row.get(0)?;
                    parse_uuid(&id)
                },
            )
            .optional()?
            .ok_or_else(|| {
                CodegraphError::InvalidInput(format!(
                    "file {path:?} is not in the current ready snapshot"
                ))
            })
    }

    /// Search exact display names in the current ready snapshot.
    ///
    /// # Errors
    /// Returns an error for an invalid query or limit, absent ready snapshot,
    /// evidence-output overflow, or `SQLite` failure.
    pub fn search_name(
        &self,
        workspace_id: Uuid,
        query: &str,
        limit: usize,
    ) -> Result<SearchResult> {
        if query.is_empty() || query.len() > MAX_NAME_BYTES {
            return Err(CodegraphError::InvalidInput(
                "name query must be non-empty and within the name byte limit".into(),
            ));
        }
        check_limit(limit, MAX_QUERY_LIMIT)?;
        let snapshot = self.current_ready(workspace_id)?;
        // Keep SQL parameter indices distinct from the name and requested bound.
        let mut statement = self.connection.prepare(
            "SELECT node_id,kind,name,path,start_byte,end_byte,start_line,start_column,end_line,end_column,provenance
             FROM cg_nodes WHERE workspace_id=?1 AND generation=?2 AND name=?3 ORDER BY path,node_id LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                workspace_id.to_string(),
                snapshot.generation,
                query,
                limit + 1
            ],
            node_from_row,
        )?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        let complete = result.len() <= limit;
        result.truncate(limit);
        attach_node_evidence(
            &self.connection,
            workspace_id,
            snapshot.generation,
            &mut result,
        )?;
        Ok(SearchResult {
            snapshot,
            requested_limit: limit,
            effective_limit: limit,
            complete,
            nodes: result,
        })
    }

    /// Search nodes in one normalized workspace-relative path.
    ///
    /// # Errors
    /// Returns an error for an invalid path or limit, absent ready snapshot,
    /// evidence-output overflow, or `SQLite` failure.
    pub fn search_path(
        &self,
        workspace_id: Uuid,
        path: &str,
        limit: usize,
    ) -> Result<SearchResult> {
        validate_path(path)?;
        check_limit(limit, MAX_QUERY_LIMIT)?;
        let snapshot = self.current_ready(workspace_id)?;
        let mut statement = self.connection.prepare(
            "SELECT node_id,kind,name,path,start_byte,end_byte,start_line,start_column,end_line,end_column,provenance
             FROM cg_nodes WHERE workspace_id=?1 AND generation=?2 AND path=?3 ORDER BY start_byte,node_id LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                workspace_id.to_string(),
                snapshot.generation,
                path,
                limit + 1
            ],
            node_from_row,
        )?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        let complete = result.len() <= limit;
        result.truncate(limit);
        attach_node_evidence(
            &self.connection,
            workspace_id,
            snapshot.generation,
            &mut result,
        )?;
        Ok(SearchResult {
            snapshot,
            requested_limit: limit,
            effective_limit: limit,
            complete,
            nodes: result,
        })
    }

    /// Explain a node with ownership, relations, evidence, and unresolved sites.
    /// If any relation, unresolved-site, evidence, or output-byte bound is
    /// exceeded, the whole query returns [`CodegraphError::LimitExceeded`].
    /// No partial explanation is returned.
    ///
    /// # Errors
    /// Returns an error for an unknown node, absent ready snapshot, result
    /// bound overflow, or `SQLite` failure.
    #[allow(clippy::too_many_lines)] // This bounded read assembles one cohesive explain projection.
    pub fn explain(&self, workspace_id: Uuid, node_id: Uuid) -> Result<ExplainView> {
        let snapshot = self.current_ready(workspace_id)?;
        let mut node = self.connection.query_row(
            "SELECT node_id,kind,name,path,start_byte,end_byte,start_line,start_column,end_line,end_column,provenance
             FROM cg_nodes WHERE workspace_id=?1 AND generation=?2 AND node_id=?3",
            params![workspace_id.to_string(), snapshot.generation, node_id.to_string()], node_from_row,
        ).optional()?.ok_or_else(|| CodegraphError::InvalidInput(format!("node {node_id} is not in the current ready snapshot")))?;
        node.evidence = load_evidence(
            &self.connection,
            workspace_id,
            snapshot.generation,
            node_id,
            "node",
        )?;
        let mut statement = self.connection.prepare(
            "SELECT relation_id,kind,source_id,target_id,provenance FROM cg_relations
             WHERE workspace_id=?1 AND generation=?2 AND (source_id=?3 OR target_id=?3)
             ORDER BY kind,source_id,target_id,relation_id LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                workspace_id.to_string(),
                snapshot.generation,
                node_id.to_string(),
                MAX_EXPLAIN_RELATIONS + 1
            ],
            |row| {
                Ok(RelationView {
                    id: parse_uuid(&row.get::<_, String>(0)?)?,
                    kind: parse_relation(&row.get::<_, String>(1)?)?,
                    source: parse_uuid(&row.get::<_, String>(2)?)?,
                    target: parse_uuid(&row.get::<_, String>(3)?)?,
                    provenance: parse_provenance(&row.get::<_, String>(4)?)?,
                    evidence: Vec::new(),
                })
            },
        )?;
        let mut relations = Vec::new();
        for row in rows {
            relations.push(row?);
        }
        if relations.len() > MAX_EXPLAIN_RELATIONS {
            return Err(CodegraphError::LimitExceeded {
                requested: relations.len(),
                maximum: MAX_EXPLAIN_RELATIONS,
            });
        }
        let mut output_bytes = 0usize;
        output_bytes = output_bytes.saturating_add(
            node.name.len()
                + node.span.path.len()
                + 160
                + node
                    .evidence
                    .iter()
                    .map(|item| item.label.len() + item.span.path.len() + 128)
                    .sum::<usize>(),
        );
        if output_bytes > MAX_QUERY_OUTPUT_BYTES {
            return Err(CodegraphError::LimitExceeded {
                requested: output_bytes,
                maximum: MAX_QUERY_OUTPUT_BYTES,
            });
        }
        for relation in &mut relations {
            relation.evidence = load_evidence(
                &self.connection,
                workspace_id,
                snapshot.generation,
                relation.id,
                "relation",
            )?;
            output_bytes = output_bytes.saturating_add(
                160 + relation
                    .evidence
                    .iter()
                    .map(|item| item.label.len() + item.span.path.len() + 128)
                    .sum::<usize>(),
            );
            if output_bytes > MAX_QUERY_OUTPUT_BYTES {
                return Err(CodegraphError::LimitExceeded {
                    requested: output_bytes,
                    maximum: MAX_QUERY_OUTPUT_BYTES,
                });
            }
        }
        let mut ownership = Vec::new();
        let mut directed_relations = Vec::new();
        for relation in relations {
            if relation.kind == RelationKind::Contains {
                ownership.push(relation);
            } else {
                directed_relations.push(relation);
            }
        }
        let unresolved_references =
            load_unresolved(&self.connection, workspace_id, snapshot.generation, node_id)?;
        output_bytes = output_bytes.saturating_add(
            unresolved_references
                .iter()
                .map(|item| item.raw_target.len() + item.span.path.len() + 160)
                .sum::<usize>(),
        );
        if output_bytes > MAX_QUERY_OUTPUT_BYTES {
            return Err(CodegraphError::LimitExceeded {
                requested: output_bytes,
                maximum: MAX_QUERY_OUTPUT_BYTES,
            });
        }
        Ok(ExplainView {
            snapshot,
            node,
            ownership,
            directed_relations,
            unresolved_references,
        })
    }
}

fn derive_workspace_id(project: Uuid, instance_key: &str) -> Uuid {
    let repository = CodegraphStore::repository_id(project);
    Uuid::new_v5(
        &WORKSPACE_NS,
        format!("{repository}\0{instance_key}").as_bytes(),
    )
}

/// Upgrade every persisted v1 generation in place, preserving ready facts.
///
/// V1 facts are reconstructable while its temporary source archive still
/// exists. Convert its noncanonical ownership vocabulary, derive v2 evidence
/// and file identities, and replace v1 digests before dropping source bytes, all
/// within the caller's migration transaction. Thus no ready head observes a
/// partially upgraded generation and no user-visible fact is discarded. V1
/// node/relation IDs remain legacy identities; their reconstructed digest
/// inputs are explicitly tagged `legacy-v1-migration`. Republished facts use
/// the current path/language/site identity domain.
#[allow(clippy::too_many_lines)] // Keep every compatibility rewrite in one transaction boundary.
fn migrate_v1_to_v2(transaction: &Transaction<'_>) -> Result<()> {
    let mut cursor: Option<(String, i64)> = None;
    loop {
        let next_snapshot = if let Some((workspace, generation)) = cursor.as_ref() {
            transaction
                .query_row(
                    "SELECT workspace_id,generation FROM cg_snapshots
                     WHERE workspace_id>?1 OR (workspace_id=?1 AND generation>?2)
                     ORDER BY workspace_id,generation LIMIT 1",
                    params![workspace, generation],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?
        } else {
            transaction
                .query_row(
                    "SELECT workspace_id,generation FROM cg_snapshots
                     ORDER BY workspace_id,generation LIMIT 1",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?
        };
        let Some((workspace, generation)) = next_snapshot else {
            break;
        };
        migrate_v1_snapshot(transaction, &workspace, generation)?;
        cursor = Some((workspace, generation));
    }

    // Source bytes were migration input only; v2 keeps digests, spans, and facts.
    transaction.execute_batch("ALTER TABLE cg_files DROP COLUMN bytes;")?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // Rewrite one capped generation within the migration transaction.
fn migrate_v1_snapshot(
    transaction: &Transaction<'_>,
    workspace: &str,
    generation: i64,
) -> Result<()> {
    let (file_count, total_bytes, largest_file): (i64, i64, i64) = transaction.query_row(
        "SELECT COUNT(*),COALESCE(SUM(length(bytes)),0),COALESCE(MAX(length(bytes)),0)
         FROM cg_files WHERE workspace_id=?1 AND generation=?2",
        params![workspace, generation],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if file_count == 0 {
        return Err(CodegraphError::InvalidInput(format!(
            "legacy snapshot {workspace}/{generation} has no source files"
        )));
    }
    let file_count = sqlite_count(file_count, "file count")?;
    let total_bytes = sqlite_count(total_bytes, "source byte count")?;
    let largest_file = sqlite_count(largest_file, "largest source file size")?;
    for (requested, maximum) in [
        (file_count, MAX_FILES),
        (total_bytes, MAX_TOTAL_SOURCE_BYTES),
        (largest_file, MAX_FILE_BYTES),
    ] {
        if requested > maximum {
            return Err(CodegraphError::LimitExceeded { requested, maximum });
        }
    }

    let (node_count, relation_count, evidence_count): (i64, i64, i64) = transaction.query_row(
        "SELECT
           (SELECT COUNT(*) FROM cg_nodes WHERE workspace_id=?1 AND generation=?2),
           (SELECT COUNT(*) FROM cg_relations WHERE workspace_id=?1 AND generation=?2),
           (SELECT COUNT(*) FROM cg_evidence WHERE workspace_id=?1 AND generation=?2)",
        params![workspace, generation],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let fact_count = sqlite_count(
        node_count.max(relation_count).max(evidence_count),
        "fact count",
    )?;
    if fact_count > MAX_FACTS {
        return Err(CodegraphError::LimitExceeded {
            requested: fact_count,
            maximum: MAX_FACTS,
        });
    }
    let migrated_evidence_count = evidence_count
        .checked_add(node_count)
        .ok_or_else(|| CodegraphError::InvalidInput("legacy evidence count overflow".into()))?;
    let migrated_evidence_count = sqlite_count(migrated_evidence_count, "migrated evidence count")?;
    if migrated_evidence_count > MAX_FACTS {
        return Err(CodegraphError::LimitExceeded {
            requested: migrated_evidence_count,
            maximum: MAX_FACTS,
        });
    }

    let repository: String = transaction.query_row(
        "SELECT repository_id FROM cg_snapshots WHERE workspace_id=?1 AND generation=?2",
        params![workspace, generation],
        |row| row.get(0),
    )?;
    let repository = parse_uuid(&repository)?;
    let files = {
        let mut statement = transaction.prepare(
            "SELECT path,bytes FROM cg_files
             WHERE workspace_id=?1 AND generation=?2 ORDER BY path",
        )?;
        statement
            .query_map(params![workspace, generation], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (path, bytes) in files {
        let file_id = Uuid::new_v5(&FILE_NS, format!("{repository}\0{path}").as_bytes());
        let source_digest = blake3::hash(&bytes).to_hex().to_string();
        transaction.execute(
            "UPDATE cg_files SET file_id=?1,source_digest=?2
             WHERE workspace_id=?3 AND generation=?4 AND path=?5",
            params![
                file_id.to_string(),
                source_digest,
                workspace,
                generation,
                path
            ],
        )?;
    }

    let legacy_ownership = {
        let mut statement = transaction.prepare(
            "SELECT relation_id,relation_key,source_id,target_id
             FROM cg_relations WHERE workspace_id=?1 AND generation=?2 AND kind='Owns'
             ORDER BY relation_id",
        )?;
        statement
            .query_map(params![workspace, generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (old_id, key, source, target) in legacy_ownership {
        let source_id = parse_uuid(&source)?;
        let target_id = parse_uuid(&target)?;
        let new_id = legacy_relation_id(
            repository,
            RelationKind::Contains,
            source_id,
            target_id,
            &key,
        );
        transaction.execute(
            "UPDATE cg_evidence SET fact_id=?1
             WHERE workspace_id=?2 AND generation=?3 AND fact_id=?4",
            params![new_id.to_string(), workspace, generation, old_id],
        )?;
        transaction.execute(
            "UPDATE cg_relations SET relation_id=?1,kind='Contains'
             WHERE workspace_id=?2 AND generation=?3 AND relation_id=?4",
            params![new_id.to_string(), workspace, generation, old_id],
        )?;
    }

    let evidence_rows = {
        let mut statement = transaction.prepare(
            "SELECT e.fact_id,e.ordinal,e.label,e.path,e.start_byte,e.end_byte,
                    e.start_line,e.start_column,e.end_line,e.end_column,f.source_digest
             FROM cg_evidence e JOIN cg_files f
               ON f.workspace_id=e.workspace_id AND f.generation=e.generation
              AND f.path=e.path
             WHERE e.workspace_id=?1 AND e.generation=?2
             ORDER BY e.fact_id,e.ordinal",
        )?;
        statement
            .query_map(params![workspace, generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    EvidenceFact {
                        label: row.get(2)?,
                        span: SourceSpan {
                            path: row.get(3)?,
                            start_byte: row.get(4)?,
                            end_byte: row.get(5)?,
                            start_line: row.get(6)?,
                            start_column: row.get(7)?,
                            end_line: row.get(8)?,
                            end_column: row.get(9)?,
                        },
                    },
                    row.get::<_, String>(10)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (fact, ordinal, evidence, source_digest) in evidence_rows {
        transaction.execute(
            "UPDATE cg_evidence SET source_digest=?1,evidence_digest=?2
             WHERE workspace_id=?3 AND generation=?4 AND fact_id=?5 AND ordinal=?6",
            params![
                source_digest,
                evidence_digest(&source_digest, &evidence),
                workspace,
                generation,
                fact,
                ordinal
            ],
        )?;
    }

    let nodes_without_evidence = {
        let mut statement = transaction.prepare(
            "SELECT n.node_id,n.path,n.start_byte,n.end_byte,n.start_line,
                    n.start_column,n.end_line,n.end_column
             FROM cg_nodes n
             WHERE n.workspace_id=?1 AND n.generation=?2 AND NOT EXISTS (
               SELECT 1 FROM cg_evidence e
               WHERE e.workspace_id=n.workspace_id AND e.generation=n.generation
                 AND e.fact_id=n.node_id AND e.fact_type='node'
             )
             ORDER BY n.node_id",
        )?;
        statement
            .query_map(params![workspace, generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    SourceSpan {
                        path: row.get(1)?,
                        start_byte: row.get(2)?,
                        end_byte: row.get(3)?,
                        start_line: row.get(4)?,
                        start_column: row.get(5)?,
                        end_line: row.get(6)?,
                        end_column: row.get(7)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (node, span) in nodes_without_evidence {
        insert_evidence(
            transaction,
            parse_uuid(workspace)?,
            generation,
            parse_uuid(&node)?,
            0,
            "node",
            &EvidenceFact {
                label: "legacy source-backed node".into(),
                span,
            },
        )?;
    }

    let workspace_id = parse_uuid(workspace)?;
    let bundle = load_snapshot_bundle(transaction, workspace_id, generation)?;
    let project_text: String = transaction.query_row(
        "SELECT project_id FROM cg_snapshots WHERE workspace_id=?1 AND generation=?2",
        params![workspace, generation],
        |row| row.get(0),
    )?;
    let project = parse_uuid(&project_text)?;
    transaction.execute(
        "UPDATE cg_snapshots SET snapshot_digest=?1,graph_digest=?2
         WHERE workspace_id=?3 AND generation=?4",
        params![
            bundle_digest(&bundle, project, repository, workspace_id),
            graph_digest(&bundle, project, repository, workspace_id),
            workspace,
            generation
        ],
    )?;

    Ok(())
}

#[allow(clippy::too_many_lines)] // Rehydrate a complete generation for canonical digest repair.
fn load_snapshot_bundle(
    connection: &Connection,
    workspace: Uuid,
    generation: i64,
) -> Result<SourceFactBundle> {
    let files = {
        let mut statement = connection.prepare(
            "SELECT path,bytes FROM cg_files
             WHERE workspace_id=?1 AND generation=?2 ORDER BY path",
        )?;
        statement
            .query_map(params![workspace.to_string(), generation], |row| {
                Ok(SourceFile {
                    relative_path: row.get(0)?,
                    bytes: row.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let node_rows = {
        let mut statement = connection.prepare(
            "SELECT node_id,node_key,kind,name,path,start_byte,end_byte,start_line,
                    start_column,end_line,end_column
             FROM cg_nodes WHERE workspace_id=?1 AND generation=?2 ORDER BY node_key",
        )?;
        statement
            .query_map(params![workspace.to_string(), generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    SourceSpan {
                        path: row.get(4)?,
                        start_byte: row.get(5)?,
                        end_byte: row.get(6)?,
                        start_line: row.get(7)?,
                        start_column: row.get(8)?,
                        end_line: row.get(9)?,
                        end_column: row.get(10)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut node_keys = std::collections::HashMap::new();
    let mut nodes = Vec::with_capacity(node_rows.len());
    for (node_id_text, key, kind, name, span) in node_rows {
        let id = parse_uuid(&node_id_text)?;
        node_keys.insert(id, key.clone());
        let evidence = load_evidence(connection, workspace, generation, id, "node")?
            .into_iter()
            .map(|item| EvidenceFact {
                label: item.label,
                span: item.span,
            })
            .collect();
        nodes.push(NodeFact {
            key: FactKey(key),
            identity: NodeIdentity {
                language: "legacy-v1".into(),
                qualified_name: node_keys[&id].clone(),
                disambiguator: "v1".into(),
            },
            kind: parse_node(&kind)?,
            name,
            span,
            provenance: FactProvenance::Extracted,
            evidence,
        });
    }
    let relation_rows = {
        let mut statement = connection.prepare(
            "SELECT relation_id,relation_key,kind,source_id,target_id
             FROM cg_relations WHERE workspace_id=?1 AND generation=?2
             ORDER BY relation_key",
        )?;
        statement
            .query_map(params![workspace.to_string(), generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut relations = Vec::with_capacity(relation_rows.len());
    for (relation_id_text, key, kind, source, target) in relation_rows {
        let id = parse_uuid(&relation_id_text)?;
        let source_id = parse_uuid(&source)?;
        let target_id = parse_uuid(&target)?;
        let source_key = node_keys.get(&source_id).ok_or_else(|| {
            CodegraphError::InvalidInput(format!("relation {key} has a missing source node"))
        })?;
        let target_key = node_keys.get(&target_id).ok_or_else(|| {
            CodegraphError::InvalidInput(format!("relation {key} has a missing target node"))
        })?;
        let evidence: Vec<EvidenceFact> =
            load_evidence(connection, workspace, generation, id, "relation")?
                .into_iter()
                .map(|item| EvidenceFact {
                    label: item.label,
                    span: item.span,
                })
                .collect();
        relations.push(RelationFact {
            site_anchor: format!("legacy-v1:{key}"),
            key: FactKey(key),
            owner_file: evidence
                .first()
                .ok_or_else(|| {
                    CodegraphError::InvalidInput("legacy relation has no evidence".into())
                })?
                .span
                .path
                .clone(),
            kind: parse_relation(&kind)?,
            source: FactKey(source_key.clone()),
            target: FactKey(target_key.clone()),
            provenance: FactProvenance::Extracted,
            evidence,
        });
    }
    Ok(SourceFactBundle {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "legacy-v1-migration".into(),
                version: "1".into(),
            },
        },
        files,
        nodes,
        relations,
        unresolved_references: Vec::new(),
    })
}

fn sqlite_count(value: i64, label: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        CodegraphError::InvalidInput(format!("legacy snapshot has invalid {label}: {value}"))
    })
}

const fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested == 0 || requested > maximum {
        return Err(CodegraphError::LimitExceeded { requested, maximum });
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
    {
        return Err(CodegraphError::InvalidInput(format!(
            "invalid workspace-relative path: {path:?}"
        )));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(CodegraphError::InvalidInput(format!(
            "path is not normalized: {path:?}"
        )));
    }
    Ok(())
}

fn validate_span(
    span: &SourceSpan,
    files: &std::collections::HashMap<&str, &SourceFile>,
) -> Result<()> {
    validate_path(&span.path)?;
    let file = files.get(span.path.as_str()).ok_or_else(|| {
        CodegraphError::InvalidInput(format!("span refers to missing source file {}", span.path))
    })?;
    if span.start_byte > span.end_byte || span.end_byte > file.bytes.len() {
        return Err(CodegraphError::InvalidInput(format!(
            "span {span:?} is outside exact source bytes"
        )));
    }
    let (start_line, start_column) = line_column(&file.bytes, span.start_byte);
    let (end_line, end_column) = line_column(&file.bytes, span.end_byte);
    if (
        span.start_line,
        span.start_column,
        span.end_line,
        span.end_column,
    ) != (start_line, start_column, end_line, end_column)
    {
        return Err(CodegraphError::InvalidInput(format!(
            "span line/byte columns do not match exact bytes: {span:?}"
        )));
    }
    Ok(())
}

fn line_column(bytes: &[u8], offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for byte in &bytes[..offset] {
        if *byte == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

#[allow(clippy::too_many_lines)] // All input invariants run before opening the publish transaction.
fn validate_bundle(bundle: &SourceFactBundle) -> Result<()> {
    validate_bundle_with_limits(
        bundle,
        MAX_FILES,
        MAX_FACTS,
        MAX_FILE_BYTES,
        MAX_TOTAL_SOURCE_BYTES,
        true,
    )
}

#[allow(clippy::too_many_lines)] // Shared strict S0/S1 fact validation with distinct resource ceilings.
fn validate_bundle_with_limits(
    bundle: &SourceFactBundle,
    max_files: usize,
    max_facts: usize,
    max_file_bytes: usize,
    max_total_bytes: usize,
    require_local_endpoints: bool,
) -> Result<()> {
    if bundle.extraction.mode != ExtractionMode::ExtractedV1_0
        || bundle.extraction.extractor.name.trim().is_empty()
        || bundle.extraction.extractor.name.len() > MAX_EXTRACTOR_NAME_BYTES
        || bundle.extraction.extractor.version.trim().is_empty()
        || bundle.extraction.extractor.version.len() > MAX_EXTRACTOR_VERSION_BYTES
        || bundle.extraction.extractor.name.contains('\0')
        || bundle.extraction.extractor.version.contains('\0')
    {
        return Err(CodegraphError::InvalidInput(
            "EXTRACTED/1.0 requires a bounded extractor name and version".into(),
        ));
    }
    if bundle.files.is_empty() || bundle.files.len() > max_files {
        return Err(CodegraphError::LimitExceeded {
            requested: bundle.files.len(),
            maximum: max_files,
        });
    }
    if bundle.nodes.len() > max_facts
        || bundle.relations.len() > max_facts
        || bundle.unresolved_references.len() > max_facts
    {
        return Err(CodegraphError::LimitExceeded {
            requested: bundle
                .nodes
                .len()
                .max(bundle.relations.len())
                .max(bundle.unresolved_references.len()),
            maximum: max_facts,
        });
    }
    let mut total = 0usize;
    let mut files = std::collections::HashMap::new();
    for file in &bundle.files {
        validate_path(&file.relative_path)?;
        if file.bytes.len() > max_file_bytes {
            return Err(CodegraphError::LimitExceeded {
                requested: file.bytes.len(),
                maximum: max_file_bytes,
            });
        }
        total = total.saturating_add(file.bytes.len());
        if files.insert(file.relative_path.as_str(), file).is_some() {
            return Err(CodegraphError::InvalidInput(format!(
                "duplicate source path {}",
                file.relative_path
            )));
        }
    }
    if total > max_total_bytes {
        return Err(CodegraphError::LimitExceeded {
            requested: total,
            maximum: max_total_bytes,
        });
    }
    let mut keys = std::collections::HashSet::new();
    let mut evidence_count = 0usize;
    for node in &bundle.nodes {
        if node.key.0.is_empty()
            || node.key.0.len() > MAX_NAME_BYTES
            || node.name.is_empty()
            || node.name.len() > MAX_NAME_BYTES
        {
            return Err(CodegraphError::InvalidInput(
                "node key and display name must be non-empty and within the name byte limit".into(),
            ));
        }
        if !keys.insert(node.key.0.as_str()) {
            return Err(CodegraphError::InvalidInput(format!(
                "duplicate fact key {}",
                node.key.0
            )));
        }
        if node.provenance != FactProvenance::Extracted
            || node.identity.language.is_empty()
            || node.identity.language.len() > MAX_LANGUAGE_BYTES
            || node.identity.qualified_name.is_empty()
            || node.identity.qualified_name.len() > MAX_NAME_BYTES
            || node.identity.disambiguator.is_empty()
            || node.identity.disambiguator.len() > MAX_NAME_BYTES
            || node.identity.language.contains('\0')
            || node.identity.qualified_name.contains('\0')
            || node.identity.disambiguator.contains('\0')
        {
            return Err(CodegraphError::InvalidInput(
                "extracted node requires validated language, qualified name and disambiguator"
                    .into(),
            ));
        }
        validate_span(&node.span, &files)?;
        validate_evidence(&node.evidence, &files)?;
        evidence_count = evidence_count.saturating_add(node.evidence.len());
    }
    let mut rel_keys = std::collections::HashSet::new();
    for relation in &bundle.relations {
        if require_local_endpoints && !keys.contains(relation.source.0.as_str()) {
            return Err(CodegraphError::MissingNode(relation.source.0.clone()));
        }
        if require_local_endpoints && !keys.contains(relation.target.0.as_str()) {
            return Err(CodegraphError::MissingNode(relation.target.0.clone()));
        }
        if relation.key.0.is_empty()
            || relation.key.0.len() > MAX_NAME_BYTES
            || !rel_keys.insert(relation.key.0.as_str())
        {
            return Err(CodegraphError::InvalidInput(format!(
                "empty or duplicate relation key {}",
                relation.key.0
            )));
        }
        validate_path(&relation.owner_file)?;
        if relation.provenance != FactProvenance::Extracted
            || !files.contains_key(relation.owner_file.as_str())
            || !valid_site_anchor(&relation.site_anchor)
            || relation.site_anchor.len() > MAX_SITE_ANCHOR_BYTES
            || relation.site_anchor.contains('\0')
            || !relation
                .evidence
                .iter()
                .any(|item| item.span.path == relation.owner_file)
        {
            return Err(CodegraphError::InvalidInput(
                "extracted relation requires an owner file, site anchor and owner evidence".into(),
            ));
        }
        if relation.evidence.len() > MAX_EVIDENCE_PER_FACT {
            return Err(CodegraphError::LimitExceeded {
                requested: relation.evidence.len(),
                maximum: MAX_EVIDENCE_PER_FACT,
            });
        }
        validate_evidence(&relation.evidence, &files)?;
        evidence_count = evidence_count.saturating_add(relation.evidence.len());
    }
    let mut unresolved_keys = std::collections::HashSet::new();
    for reference in &bundle.unresolved_references {
        if !keys.contains(reference.owner.0.as_str()) {
            return Err(CodegraphError::MissingNode(reference.owner.0.clone()));
        }
        if reference.key.0.is_empty()
            || reference.key.0.len() > MAX_NAME_BYTES
            || !unresolved_keys.insert(reference.key.0.as_str())
        {
            return Err(CodegraphError::InvalidInput(format!(
                "empty or duplicate unresolved site key {}",
                reference.key.0
            )));
        }
        if reference.raw_target.is_empty()
            || reference.raw_target.len() > MAX_UNRESOLVED_TARGET_BYTES
        {
            return Err(CodegraphError::InvalidInput(
                "unresolved raw target is empty or too large".into(),
            ));
        }
        if reference.provenance != FactProvenance::Extracted {
            return Err(CodegraphError::InvalidInput(
                "EXTRACTED/1.0 unresolved sites require extracted provenance".into(),
            ));
        }
        if reference
            .span
            .end_byte
            .saturating_sub(reference.span.start_byte)
            > MAX_EVIDENCE_SPAN_BYTES
        {
            return Err(CodegraphError::LimitExceeded {
                requested: reference
                    .span
                    .end_byte
                    .saturating_sub(reference.span.start_byte),
                maximum: MAX_EVIDENCE_SPAN_BYTES,
            });
        }
        validate_span(&reference.span, &files)?;
    }
    if evidence_count > max_facts {
        return Err(CodegraphError::LimitExceeded {
            requested: evidence_count,
            maximum: max_facts,
        });
    }
    Ok(())
}

fn valid_site_anchor(anchor: &str) -> bool {
    let Some((version, path)) = anchor.split_once(':') else {
        return false;
    };
    let Some(number) = version.strip_prefix('v') else {
        return false;
    };
    !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && !path.is_empty()
        && !path.contains('\0')
}

fn validate_evidence(
    evidence: &[EvidenceFact],
    files: &std::collections::HashMap<&str, &SourceFile>,
) -> Result<()> {
    if evidence.is_empty() {
        return Err(CodegraphError::InvalidInput(
            "every source-backed fact requires at least one evidence record".into(),
        ));
    }
    if evidence.len() > MAX_EVIDENCE_PER_FACT {
        return Err(CodegraphError::LimitExceeded {
            requested: evidence.len(),
            maximum: MAX_EVIDENCE_PER_FACT,
        });
    }
    for item in evidence {
        if item.label.is_empty() || item.label.len() > MAX_EVIDENCE_LABEL_BYTES {
            return Err(CodegraphError::InvalidInput(
                "evidence label is empty or too large".into(),
            ));
        }
        let span_bytes = item.span.end_byte.saturating_sub(item.span.start_byte);
        if span_bytes > MAX_EVIDENCE_SPAN_BYTES {
            return Err(CodegraphError::LimitExceeded {
                requested: span_bytes,
                maximum: MAX_EVIDENCE_SPAN_BYTES,
            });
        }
        validate_span(&item.span, files)?;
    }
    Ok(())
}

fn length_field(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

fn bundle_digest(
    bundle: &SourceFactBundle,
    project: Uuid,
    repository: Uuid,
    workspace: Uuid,
) -> String {
    canonical_digest(bundle, project, repository, workspace, true)
}

fn canonical_digest(
    bundle: &SourceFactBundle,
    project: Uuid,
    repository: Uuid,
    workspace: Uuid,
    include_source_bytes: bool,
) -> String {
    let mut output = Vec::new();
    let mut files = bundle.files.iter().collect::<Vec<_>>();
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    output.extend_from_slice(b"rsi-codegraph-canonical-v3");
    length_field(
        &mut output,
        if include_source_bytes {
            b"snapshot-bytes"
        } else {
            b"graph-source-hash"
        },
    );
    length_field(&mut output, project.as_bytes());
    length_field(&mut output, repository.as_bytes());
    length_field(&mut output, workspace.as_bytes());
    length_field(&mut output, bundle.extraction.mode.as_str().as_bytes());
    length_field(&mut output, bundle.extraction.extractor.name.as_bytes());
    length_field(&mut output, bundle.extraction.extractor.version.as_bytes());
    length_field(&mut output, b"files");
    output.extend_from_slice(&(files.len() as u64).to_be_bytes());
    for file in files {
        length_field(&mut output, file.relative_path.as_bytes());
        if include_source_bytes {
            length_field(&mut output, &file.bytes);
        } else {
            length_field(&mut output, blake3::hash(&file.bytes).as_bytes());
        }
    }
    let mut nodes = bundle.nodes.iter().collect::<Vec<_>>();
    nodes.sort_by(|a, b| a.key.0.cmp(&b.key.0));
    length_field(&mut output, b"nodes");
    output.extend_from_slice(&(nodes.len() as u64).to_be_bytes());
    for node in nodes {
        length_field(&mut output, node.key.0.as_bytes());
        length_field(&mut output, node.identity.language.as_bytes());
        length_field(&mut output, node.identity.qualified_name.as_bytes());
        length_field(&mut output, node.identity.disambiguator.as_bytes());
        length_field(&mut output, node.kind.as_str().as_bytes());
        length_field(&mut output, node.provenance.as_str().as_bytes());
        length_field(&mut output, node.name.as_bytes());
        encode_span(&mut output, &node.span);
        encode_evidence(&mut output, &node.evidence);
    }
    let mut relations = bundle.relations.iter().collect::<Vec<_>>();
    relations.sort_by(|a, b| a.key.0.cmp(&b.key.0));
    length_field(&mut output, b"relations");
    output.extend_from_slice(&(relations.len() as u64).to_be_bytes());
    for relation in relations {
        length_field(&mut output, relation.key.0.as_bytes());
        length_field(&mut output, relation.owner_file.as_bytes());
        length_field(&mut output, relation.site_anchor.as_bytes());
        length_field(&mut output, relation.kind.as_str().as_bytes());
        length_field(&mut output, relation.provenance.as_str().as_bytes());
        length_field(&mut output, relation.source.0.as_bytes());
        length_field(&mut output, relation.target.0.as_bytes());
        let mut evidence = relation.evidence.iter().collect::<Vec<_>>();
        evidence.sort_by(|a, b| evidence_order(a, b));
        encode_sorted_evidence(&mut output, evidence);
    }
    let mut unresolved = bundle.unresolved_references.iter().collect::<Vec<_>>();
    unresolved.sort_by(|a, b| a.key.0.cmp(&b.key.0));
    length_field(&mut output, b"unresolved");
    output.extend_from_slice(&(unresolved.len() as u64).to_be_bytes());
    for reference in unresolved {
        length_field(&mut output, reference.key.0.as_bytes());
        length_field(&mut output, reference.provenance.as_str().as_bytes());
        length_field(&mut output, reference.owner.0.as_bytes());
        length_field(&mut output, reference.kind.as_str().as_bytes());
        length_field(&mut output, reference.raw_target.as_bytes());
        encode_span(&mut output, &reference.span);
    }
    blake3::hash(&output).to_hex().to_string()
}

fn graph_digest(
    bundle: &SourceFactBundle,
    project: Uuid,
    repository: Uuid,
    workspace: Uuid,
) -> String {
    canonical_digest(bundle, project, repository, workspace, false)
}

fn evidence_order(a: &EvidenceFact, b: &EvidenceFact) -> std::cmp::Ordering {
    (&a.span.path, a.span.start_byte, a.span.end_byte, &a.label).cmp(&(
        &b.span.path,
        b.span.start_byte,
        b.span.end_byte,
        &b.label,
    ))
}

fn encode_sorted_evidence(output: &mut Vec<u8>, evidence: Vec<&EvidenceFact>) {
    for item in evidence {
        length_field(output, item.label.as_bytes());
        encode_span(output, &item.span);
    }
}

fn encode_evidence(output: &mut Vec<u8>, evidence: &[EvidenceFact]) {
    let mut items = evidence.iter().collect::<Vec<_>>();
    items.sort_by(|a, b| evidence_order(a, b));
    encode_sorted_evidence(output, items);
}

fn encode_span(output: &mut Vec<u8>, span: &SourceSpan) {
    length_field(output, span.path.as_bytes());
    for value in [
        span.start_byte,
        span.end_byte,
        span.start_line,
        span.start_column,
        span.end_line,
        span.end_column,
    ] {
        output.extend_from_slice(&(value as u64).to_be_bytes());
    }
}

fn node_id(repository_id: Uuid, node: &NodeFact) -> Uuid {
    Uuid::new_v5(
        &NODE_NS,
        format!(
            "{repository_id}\0{}\0{}\0{}\0{}\0{}",
            node.identity.language,
            node.kind.as_str(),
            node.span.path,
            node.identity.qualified_name,
            node.identity.disambiguator,
        )
        .as_bytes(),
    )
}

#[cfg(test)]
fn legacy_node_id(project: Uuid, repository: Uuid, kind: NodeKind, key: &str) -> Uuid {
    Uuid::new_v5(
        &NODE_NS,
        format!("{project}\0{repository}\0{}\0{key}", kind.as_str()).as_bytes(),
    )
}

fn relation_id(
    repository_id: Uuid,
    kind: RelationKind,
    source: Uuid,
    target: Uuid,
    owner_file: &str,
    site_anchor: &str,
) -> Uuid {
    Uuid::new_v5(
        &RELATION_NS,
        format!(
            "{repository_id}\0{}\0{source}\0{target}\0{owner_file}\0{site_anchor}",
            kind.as_str(),
        )
        .as_bytes(),
    )
}

fn legacy_relation_id(
    repository: Uuid,
    kind: RelationKind,
    source: Uuid,
    target: Uuid,
    key: &str,
) -> Uuid {
    Uuid::new_v5(
        &RELATION_NS,
        format!("{repository}\0{}\0{source}\0{target}\0{key}", kind.as_str()).as_bytes(),
    )
}

#[allow(clippy::too_many_lines)] // Installs one validated bundle within its atomic publish transaction.
fn insert_bundle(
    transaction: &Transaction<'_>,
    workspace: Uuid,
    generation: i64,
    bundle: &SourceFactBundle,
) -> Result<()> {
    let repository_id: String = transaction.query_row(
        "SELECT repository_id FROM cg_snapshots WHERE workspace_id=?1 AND generation=?2",
        params![workspace.to_string(), generation],
        |row| row.get(0),
    )?;
    let repository_id = parse_uuid(&repository_id)?;
    for file in &bundle.files {
        let digest = blake3::hash(&file.bytes).to_hex().to_string();
        let id = Uuid::new_v5(
            &FILE_NS,
            format!("{repository_id}\0{}", file.relative_path).as_bytes(),
        );
        transaction.execute(
            "INSERT INTO cg_files(workspace_id,generation,path,file_id,source_digest) VALUES (?1,?2,?3,?4,?5)",
            params![
                workspace.to_string(),
                generation,
                file.relative_path,
                id.to_string(),
                digest
            ],
        )?;
    }
    let ids = bundle
        .nodes
        .iter()
        .map(|node| (node.key.0.as_str(), node_id(repository_id, node)))
        .collect::<std::collections::HashMap<_, _>>();
    for node in &bundle.nodes {
        let id = ids[node.key.0.as_str()];
        let span = &node.span;
        transaction.execute(
            "INSERT INTO cg_nodes(workspace_id,generation,node_id,node_key,kind,name,path,start_byte,end_byte,start_line,start_column,end_line,end_column,provenance) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                workspace.to_string(),
                generation,
                id.to_string(),
                node.key.0,
                node.kind.as_str(),
                node.name,
                span.path,
                span.start_byte,
                span.end_byte,
                span.start_line,
                span.start_column,
                span.end_line,
                span.end_column,
                node.provenance.as_str(),
            ],
        )?;
        let mut evidence = node.evidence.iter().collect::<Vec<_>>();
        evidence.sort_by(|a, b| evidence_order(a, b));
        for (ordinal, evidence) in evidence.into_iter().enumerate() {
            insert_evidence(
                transaction,
                workspace,
                generation,
                id,
                ordinal,
                "node",
                evidence,
            )?;
        }
    }
    index_fts_generation(transaction, workspace, generation)?;
    for relation in &bundle.relations {
        let source_id = ids[relation.source.0.as_str()];
        let target_id = ids[relation.target.0.as_str()];
        let id = relation_id(
            repository_id,
            relation.kind,
            source_id,
            target_id,
            &relation.owner_file,
            &relation.site_anchor,
        );
        transaction.execute(
            "INSERT INTO cg_relations(workspace_id,generation,relation_id,relation_key,kind,source_id,target_id,provenance) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                workspace.to_string(),
                generation,
                id.to_string(),
                relation.key.0,
                relation.kind.as_str(),
                source_id.to_string(),
                target_id.to_string(),
                relation.provenance.as_str(),
            ],
        )?;
        let mut evidence = relation.evidence.iter().collect::<Vec<_>>();
        evidence.sort_by(|a, b| evidence_order(a, b));
        for (ordinal, evidence) in evidence.into_iter().enumerate() {
            insert_evidence(
                transaction,
                workspace,
                generation,
                id,
                ordinal,
                "relation",
                evidence,
            )?;
        }
    }
    for reference in &bundle.unresolved_references {
        let owner_id = ids[reference.owner.0.as_str()];
        let id = Uuid::new_v5(
            &RELATION_NS,
            format!("{workspace}\0unresolved\0{}", reference.key.0).as_bytes(),
        );
        let source_digest: String = transaction.query_row(
            "SELECT source_digest FROM cg_files WHERE workspace_id=?1 AND generation=?2 AND path=?3",
            params![workspace.to_string(), generation, reference.span.path],
            |row| row.get(0),
        )?;
        let digest = unresolved_digest(&source_digest, reference);
        let span = &reference.span;
        transaction.execute(
            "INSERT INTO cg_unresolved_references VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![workspace.to_string(), generation, id.to_string(), reference.key.0, reference.kind.as_str(), owner_id.to_string(), reference.raw_target, span.path, span.start_byte, span.end_byte, span.start_line, span.start_column, span.end_line, span.end_column, source_digest, digest, reference.provenance.as_str()],
        )?;
    }
    Ok(())
}

/// Materialize FTS rows from the exact source-owned node rows of one generation.
/// Called before its ready-head flip by both publication paths.
fn index_fts_generation(
    transaction: &Transaction<'_>,
    workspace: Uuid,
    generation: i64,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO cg_fts_nodes(name,path,workspace_id,generation,node_id,owner_path)
         SELECT name,path,workspace_id,generation,node_id,path FROM cg_nodes
         WHERE workspace_id=?1 AND generation=?2 ORDER BY path,start_byte,node_id",
        params![workspace.to_string(), generation],
    )?;
    Ok(())
}

fn insert_evidence(
    transaction: &Transaction<'_>,
    workspace: Uuid,
    generation: i64,
    fact: Uuid,
    ordinal: usize,
    fact_type: &str,
    evidence: &EvidenceFact,
) -> Result<()> {
    let span = &evidence.span;
    let source_digest: String = transaction.query_row(
        "SELECT source_digest FROM cg_files WHERE workspace_id=?1 AND generation=?2 AND path=?3",
        params![workspace.to_string(), generation, span.path],
        |row| row.get(0),
    )?;
    let evidence_digest = evidence_digest(&source_digest, evidence);
    transaction.execute(
        "INSERT INTO cg_evidence(workspace_id,generation,fact_id,ordinal,label,path,start_byte,end_byte,start_line,start_column,end_line,end_column,source_digest,evidence_digest,fact_type) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            workspace.to_string(),
            generation,
            fact.to_string(),
            ordinal,
            evidence.label,
            span.path,
            span.start_byte,
            span.end_byte,
            span.start_line,
            span.start_column,
            span.end_line,
            span.end_column,
            source_digest,
            evidence_digest,
            fact_type
        ],
    )?;
    Ok(())
}

fn evidence_digest(source_digest: &str, evidence: &EvidenceFact) -> String {
    let mut value = Vec::new();
    length_field(&mut value, source_digest.as_bytes());
    length_field(&mut value, evidence.label.as_bytes());
    encode_span(&mut value, &evidence.span);
    blake3::hash(&value).to_hex().to_string()
}

fn unresolved_digest(source_digest: &str, reference: &UnresolvedReferenceFact) -> String {
    let mut value = Vec::new();
    length_field(&mut value, source_digest.as_bytes());
    length_field(&mut value, reference.key.0.as_bytes());
    length_field(&mut value, reference.owner.0.as_bytes());
    length_field(&mut value, reference.kind.as_str().as_bytes());
    length_field(&mut value, reference.raw_target.as_bytes());
    encode_span(&mut value, &reference.span);
    blake3::hash(&value).to_hex().to_string()
}

fn read_snapshot(
    connection: &Connection,
    workspace_id: Uuid,
    generation: i64,
) -> Result<ReadySnapshot> {
    connection.query_row("SELECT project_id,repository_id,workspace_id,generation,snapshot_digest,graph_digest,extraction_mode,extractor_name,extractor_version FROM cg_snapshots WHERE workspace_id=?1 AND generation=?2 AND ready=1", params![workspace_id.to_string(), generation], |row| {
        Ok(ReadySnapshot { project_id: parse_uuid(&row.get::<_, String>(0)?)?, repository_id: parse_uuid(&row.get::<_, String>(1)?)?, workspace_id: parse_uuid(&row.get::<_, String>(2)?)?, generation: row.get(3)?, snapshot_digest: row.get(4)?, graph_digest: row.get(5)?, extraction: ExtractionContract { mode: parse_extraction_mode(&row.get::<_, String>(6)?)?, extractor: ExtractorIdentity { name: row.get(7)?, version: row.get(8)? } } })
    }).map_err(CodegraphError::from)
}

fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodeView> {
    Ok(NodeView {
        id: parse_uuid(&row.get::<_, String>(0)?)?,
        kind: parse_node(&row.get::<_, String>(1)?)?,
        name: row.get(2)?,
        span: SourceSpan {
            path: row.get(3)?,
            start_byte: row.get(4)?,
            end_byte: row.get(5)?,
            start_line: row.get(6)?,
            start_column: row.get(7)?,
            end_line: row.get(8)?,
            end_column: row.get(9)?,
        },
        provenance: parse_provenance(&row.get::<_, String>(10)?)?,
        evidence: Vec::new(),
    })
}

fn load_evidence(
    connection: &Connection,
    workspace: Uuid,
    generation: i64,
    fact: Uuid,
    fact_type: &str,
) -> Result<Vec<EvidenceView>> {
    let mut statement = connection.prepare("SELECT label,path,start_byte,end_byte,start_line,start_column,end_line,end_column,source_digest,evidence_digest FROM cg_evidence WHERE workspace_id=?1 AND generation=?2 AND fact_id=?3 AND fact_type=?4 ORDER BY path,start_byte,end_byte,label,ordinal LIMIT ?5")?;
    let rows = statement.query_map(
        params![
            workspace.to_string(),
            generation,
            fact.to_string(),
            fact_type,
            MAX_EVIDENCE_PER_FACT + 1,
        ],
        |row| {
            Ok(EvidenceView {
                label: row.get(0)?,
                span: SourceSpan {
                    path: row.get(1)?,
                    start_byte: row.get(2)?,
                    end_byte: row.get(3)?,
                    start_line: row.get(4)?,
                    start_column: row.get(5)?,
                    end_line: row.get(6)?,
                    end_column: row.get(7)?,
                },
                source_digest: row.get(8)?,
                evidence_digest: row.get(9)?,
            })
        },
    )?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    if result.len() > MAX_EVIDENCE_PER_FACT {
        return Err(CodegraphError::LimitExceeded {
            requested: result.len(),
            maximum: MAX_EVIDENCE_PER_FACT,
        });
    }
    Ok(result)
}

fn attach_node_evidence(
    connection: &Connection,
    workspace: Uuid,
    generation: i64,
    nodes: &mut [NodeView],
) -> Result<()> {
    let mut output_bytes = 0usize;
    for node in nodes {
        node.evidence = load_evidence(connection, workspace, generation, node.id, "node")?;
        output_bytes = output_bytes.saturating_add(
            node.name.len()
                + node.span.path.len()
                + 160
                + node
                    .evidence
                    .iter()
                    .map(|item| item.label.len() + item.span.path.len() + 128)
                    .sum::<usize>(),
        );
        if output_bytes > MAX_QUERY_OUTPUT_BYTES {
            return Err(CodegraphError::LimitExceeded {
                requested: output_bytes,
                maximum: MAX_QUERY_OUTPUT_BYTES,
            });
        }
    }
    Ok(())
}

fn load_unresolved(
    connection: &Connection,
    workspace: Uuid,
    generation: i64,
    owner: Uuid,
) -> Result<Vec<UnresolvedReferenceView>> {
    let mut statement = connection.prepare(
        "SELECT kind,owner_id,raw_target,path,start_byte,end_byte,start_line,start_column,end_line,end_column,source_digest,reference_digest
         FROM cg_unresolved_references WHERE workspace_id=?1 AND generation=?2 AND owner_id=?3
         ORDER BY path,start_byte,unresolved_id LIMIT ?4",
    )?;
    let rows = statement.query_map(
        params![
            workspace.to_string(),
            generation,
            owner.to_string(),
            MAX_EXPLAIN_UNRESOLVED + 1
        ],
        |row| {
            Ok(UnresolvedReferenceView {
                kind: parse_unresolved_kind(&row.get::<_, String>(0)?)?,
                owner: parse_uuid(&row.get::<_, String>(1)?)?,
                raw_target: row.get(2)?,
                span: SourceSpan {
                    path: row.get(3)?,
                    start_byte: row.get(4)?,
                    end_byte: row.get(5)?,
                    start_line: row.get(6)?,
                    start_column: row.get(7)?,
                    end_line: row.get(8)?,
                    end_column: row.get(9)?,
                },
                source_digest: row.get(10)?,
                reference_digest: row.get(11)?,
            })
        },
    )?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    if result.len() > MAX_EXPLAIN_UNRESOLVED {
        return Err(CodegraphError::LimitExceeded {
            requested: result.len(),
            maximum: MAX_EXPLAIN_UNRESOLVED,
        });
    }
    Ok(result)
}

fn parse_uuid(text: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn parse_node(text: &str) -> rusqlite::Result<NodeKind> {
    match text {
        "Workspace" => Ok(NodeKind::Workspace),
        "Crate" => Ok(NodeKind::Crate),
        "File" => Ok(NodeKind::File),
        "Module" => Ok(NodeKind::Module),
        "Function" => Ok(NodeKind::Function),
        "Method" => Ok(NodeKind::Method),
        "Struct" => Ok(NodeKind::Struct),
        "Trait" => Ok(NodeKind::Trait),
        "Enum" => Ok(NodeKind::Enum),
        "EnumVariant" => Ok(NodeKind::EnumVariant),
        "TypeAlias" => Ok(NodeKind::TypeAlias),
        "Const" => Ok(NodeKind::Const),
        "Static" => Ok(NodeKind::Static),
        "Impl" => Ok(NodeKind::Impl),
        "Test" => Ok(NodeKind::Test),
        "CargoTarget" => Ok(NodeKind::CargoTarget),
        "MarkdownDocument" => Ok(NodeKind::MarkdownDocument),
        "MarkdownHeading" => Ok(NodeKind::MarkdownHeading),
        "StageContract" => Ok(NodeKind::StageContract),
        "ResearchFinding" => Ok(NodeKind::ResearchFinding),
        "PlanItem" => Ok(NodeKind::PlanItem),
        "DecisionRecord" => Ok(NodeKind::DecisionRecord),
        "RationaleMarker" => Ok(NodeKind::RationaleMarker),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn parse_provenance(text: &str) -> rusqlite::Result<FactProvenance> {
    match text {
        "Extracted" => Ok(FactProvenance::Extracted),
        "Inferred" => Ok(FactProvenance::Inferred),
        "Ambiguous" => Ok(FactProvenance::Ambiguous),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn parse_extraction_mode(text: &str) -> rusqlite::Result<ExtractionMode> {
    match text {
        STRICT_EXTRACTION_MODE => Ok(ExtractionMode::ExtractedV1_0),
        "EXPLORATORY/1.0" => Ok(ExtractionMode::Exploratory),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn parse_relation(text: &str) -> rusqlite::Result<RelationKind> {
    match text {
        "Contains" => Ok(RelationKind::Contains),
        "Declares" => Ok(RelationKind::Declares),
        "Calls" => Ok(RelationKind::Calls),
        "UsesType" => Ok(RelationKind::UsesType),
        "Defines" => Ok(RelationKind::Defines),
        "Implements" => Ok(RelationKind::Implements),
        "HasMethod" => Ok(RelationKind::HasMethod),
        "Imports" => Ok(RelationKind::Imports),
        "DependsOn" => Ok(RelationKind::DependsOn),
        "DevDependsOn" => Ok(RelationKind::DevDependsOn),
        "BuildDependsOn" => Ok(RelationKind::BuildDependsOn),
        "Tests" => Ok(RelationKind::Tests),
        "Documents" => Ok(RelationKind::Documents),
        "ReferencesDoc" => Ok(RelationKind::ReferencesDoc),
        "ReferencesFinding" => Ok(RelationKind::ReferencesFinding),
        "SatisfiesFinding" => Ok(RelationKind::SatisfiesFinding),
        "Supersedes" => Ok(RelationKind::Supersedes),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn parse_unresolved_kind(text: &str) -> rusqlite::Result<UnresolvedReferenceKind> {
    match text {
        "Call" => Ok(UnresolvedReferenceKind::Call),
        "Type" => Ok(UnresolvedReferenceKind::Type),
        "ImplTrait" => Ok(UnresolvedReferenceKind::ImplTrait),
        "Import" => Ok(UnresolvedReferenceKind::Import),
        "Finding" => Ok(UnresolvedReferenceKind::Finding),
        "Document" => Ok(UnresolvedReferenceKind::Document),
        "Other" => Ok(UnresolvedReferenceKind::Other),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Panics keep fixture failures concise and local.
mod tests {
    use super::*;

    const SOURCE: &str =
        "mod sample { fn alpha() { beta(); beta(); missing_helper(); } fn beta() {} }\n";

    fn span(path: &str, text: &str, occurrence: usize) -> SourceSpan {
        let mut matches = SOURCE.match_indices(text).skip(occurrence);
        let (start, _) = matches.next().expect("fixture occurrence exists");
        let end = start + text.len();
        let (start_line, start_column) = line_column(SOURCE.as_bytes(), start);
        let (end_line, end_column) = line_column(SOURCE.as_bytes(), end);
        SourceSpan {
            path: path.into(),
            start_byte: start,
            end_byte: end,
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    #[allow(clippy::too_many_lines)] // Fixture names each structural fact and exact source site.
    fn fixture() -> SourceFactBundle {
        let path = "src/lib.rs";
        SourceFactBundle {
            extraction: ExtractionContract {
                mode: ExtractionMode::ExtractedV1_0,
                extractor: ExtractorIdentity {
                    name: "hand-authored-rust-fixture".into(),
                    version: "1.0.0".into(),
                },
            },
            files: vec![SourceFile {
                relative_path: path.into(),
                bytes: SOURCE.as_bytes().to_vec(),
            }],
            nodes: vec![
                NodeFact {
                    key: FactKey("sample".into()),
                    identity: NodeIdentity {
                        language: "rust".into(),
                        qualified_name: "sample".into(),
                        disambiguator: "module-v1".into(),
                    },
                    kind: NodeKind::Module,
                    name: "sample".into(),
                    span: span(path, "sample", 0),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![EvidenceFact {
                        label: "syntax declaration".into(),
                        span: span(path, "sample", 0),
                    }],
                },
                NodeFact {
                    key: FactKey("sample::alpha".into()),
                    identity: NodeIdentity {
                        language: "rust".into(),
                        qualified_name: "sample::alpha".into(),
                        disambiguator: "function-v1".into(),
                    },
                    kind: NodeKind::Function,
                    name: "alpha".into(),
                    span: span(path, "alpha", 0),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![
                        EvidenceFact {
                            label: "syntax declaration".into(),
                            span: span(path, "alpha", 0),
                        },
                        EvidenceFact {
                            label: "syntax declaration".into(),
                            span: span(path, "fn alpha", 0),
                        },
                    ],
                },
                NodeFact {
                    key: FactKey("sample::beta".into()),
                    identity: NodeIdentity {
                        language: "rust".into(),
                        qualified_name: "sample::beta".into(),
                        disambiguator: "function-v1".into(),
                    },
                    kind: NodeKind::Function,
                    name: "beta".into(),
                    span: span(path, "beta", 2),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![EvidenceFact {
                        label: "syntax declaration".into(),
                        span: span(path, "beta", 2),
                    }],
                },
            ],
            relations: vec![
                RelationFact {
                    key: FactKey("contains-alpha".into()),
                    owner_file: path.into(),
                    site_anchor: "v1:module/alpha".into(),
                    kind: RelationKind::Contains,
                    source: FactKey("sample".into()),
                    target: FactKey("sample::alpha".into()),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![EvidenceFact {
                        label: "module ownership".into(),
                        span: span(path, "alpha", 0),
                    }],
                },
                RelationFact {
                    key: FactKey("contains-beta".into()),
                    owner_file: path.into(),
                    site_anchor: "v1:module/beta".into(),
                    kind: RelationKind::Contains,
                    source: FactKey("sample".into()),
                    target: FactKey("sample::beta".into()),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![EvidenceFact {
                        label: "module ownership".into(),
                        span: span(path, "beta", 2),
                    }],
                },
                RelationFact {
                    key: FactKey("call-site-1".into()),
                    owner_file: path.into(),
                    site_anchor: "v1:alpha/call-1".into(),
                    kind: RelationKind::Calls,
                    source: FactKey("sample::alpha".into()),
                    target: FactKey("sample::beta".into()),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![
                        EvidenceFact {
                            label: "call expression".into(),
                            span: span(path, "beta", 0),
                        },
                        EvidenceFact {
                            label: "call expression".into(),
                            span: span(path, "beta", 1),
                        },
                    ],
                },
                RelationFact {
                    key: FactKey("call-site-2".into()),
                    owner_file: path.into(),
                    site_anchor: "v1:alpha/call-2".into(),
                    kind: RelationKind::Calls,
                    source: FactKey("sample::alpha".into()),
                    target: FactKey("sample::beta".into()),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![EvidenceFact {
                        label: "call expression".into(),
                        span: span(path, "beta", 0),
                    }],
                },
            ],
            unresolved_references: vec![UnresolvedReferenceFact {
                key: FactKey("alpha::missing-helper-site".into()),
                owner: FactKey("sample::alpha".into()),
                kind: UnresolvedReferenceKind::Call,
                raw_target: "missing_helper".into(),
                span: span(path, "missing_helper", 0),
                provenance: FactProvenance::Extracted,
            }],
        }
    }

    fn store() -> (tempfile::TempDir, CodegraphStore, Uuid, Uuid) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let project = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let store = CodegraphStore::open(directory.path().join("codegraph.sqlite"), project)
            .expect("store opens");
        let workspace = CodegraphStore::workspace_id(project, "primary").unwrap();
        (directory, store, project, workspace)
    }

    fn publish_primary(
        store: &mut CodegraphStore,
        bundle: &SourceFactBundle,
    ) -> Result<ReadySnapshot> {
        let scope = store.scope(WorkspaceInstanceKey::Primary);
        store.publish(&scope, bundle)
    }

    #[allow(clippy::too_many_lines)] // Mirrors the released v1 schema with populated ready heads.
    fn create_populated_v1_database(path: &Path, project: Uuid, workspaces: &[Uuid]) -> Vec<i64> {
        let connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cg_store_meta (project_id TEXT PRIMARY KEY);
                CREATE TABLE cg_snapshots (
                    project_id TEXT NOT NULL, repository_id TEXT NOT NULL,
                    workspace_id TEXT NOT NULL, generation INTEGER NOT NULL,
                    snapshot_digest TEXT NOT NULL, graph_digest TEXT NOT NULL,
                    ready INTEGER NOT NULL CHECK (ready IN (0,1)),
                    PRIMARY KEY (workspace_id,generation)
                );
                CREATE TABLE cg_workspace_heads (
                    workspace_id TEXT PRIMARY KEY, generation INTEGER NOT NULL,
                    FOREIGN KEY (workspace_id,generation)
                      REFERENCES cg_snapshots(workspace_id,generation)
                );
                CREATE TABLE cg_files (
                    workspace_id TEXT NOT NULL, generation INTEGER NOT NULL,
                    path TEXT NOT NULL, file_id TEXT NOT NULL, bytes BLOB NOT NULL,
                    source_digest TEXT NOT NULL,
                    PRIMARY KEY (workspace_id,generation,path),
                    FOREIGN KEY (workspace_id,generation)
                      REFERENCES cg_snapshots(workspace_id,generation)
                );
                CREATE TABLE cg_nodes (
                    workspace_id TEXT NOT NULL, generation INTEGER NOT NULL,
                    node_id TEXT NOT NULL, node_key TEXT NOT NULL, kind TEXT NOT NULL,
                    name TEXT NOT NULL, path TEXT NOT NULL, start_byte INTEGER NOT NULL,
                    end_byte INTEGER NOT NULL, start_line INTEGER NOT NULL,
                    start_column INTEGER NOT NULL, end_line INTEGER NOT NULL,
                    end_column INTEGER NOT NULL,
                    PRIMARY KEY (workspace_id,generation,node_id),
                    UNIQUE (workspace_id,generation,node_key),
                    FOREIGN KEY (workspace_id,generation,path)
                      REFERENCES cg_files(workspace_id,generation,path)
                );
                CREATE INDEX cg_nodes_name ON cg_nodes(workspace_id,generation,name);
                CREATE INDEX cg_nodes_path ON cg_nodes(workspace_id,generation,path);
                CREATE TABLE cg_relations (
                    workspace_id TEXT NOT NULL, generation INTEGER NOT NULL,
                    relation_id TEXT NOT NULL, relation_key TEXT NOT NULL,
                    kind TEXT NOT NULL, source_id TEXT NOT NULL, target_id TEXT NOT NULL,
                    PRIMARY KEY (workspace_id,generation,relation_id),
                    UNIQUE (workspace_id,generation,relation_key),
                    FOREIGN KEY (workspace_id,generation,source_id)
                      REFERENCES cg_nodes(workspace_id,generation,node_id),
                    FOREIGN KEY (workspace_id,generation,target_id)
                      REFERENCES cg_nodes(workspace_id,generation,node_id)
                );
                CREATE INDEX cg_relations_endpoints
                  ON cg_relations(workspace_id,generation,source_id,target_id);
                CREATE TABLE cg_evidence (
                    workspace_id TEXT NOT NULL, generation INTEGER NOT NULL,
                    fact_id TEXT NOT NULL, ordinal INTEGER NOT NULL, label TEXT NOT NULL,
                    path TEXT NOT NULL, start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
                    start_line INTEGER NOT NULL, start_column INTEGER NOT NULL,
                    end_line INTEGER NOT NULL, end_column INTEGER NOT NULL,
                    source_digest TEXT NOT NULL, evidence_digest TEXT NOT NULL,
                    PRIMARY KEY (workspace_id,generation,fact_id,ordinal),
                    FOREIGN KEY (workspace_id,generation,path)
                      REFERENCES cg_files(workspace_id,generation,path)
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO cg_store_meta(project_id) VALUES (?1)",
                [project.to_string()],
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();

        let repository = CodegraphStore::repository_id(project);
        let source = SOURCE.as_bytes();
        let source_digest = blake3::hash(source).to_hex().to_string();
        let node_facts = [
            (
                "sample",
                NodeKind::Module,
                "sample",
                span("src/lib.rs", "sample", 0),
            ),
            (
                "sample::alpha",
                NodeKind::Function,
                "alpha",
                span("src/lib.rs", "alpha", 0),
            ),
            (
                "sample::beta",
                NodeKind::Function,
                "beta",
                span("src/lib.rs", "beta", 2),
            ),
        ];
        let ids = node_facts
            .iter()
            .map(|(key, kind, _, _)| (*key, legacy_node_id(project, repository, *kind, key)))
            .collect::<std::collections::HashMap<_, _>>();
        let generation_by_workspace = workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                let generation = 3 + i64::try_from(index).unwrap();
                connection
                    .execute(
                        "INSERT INTO cg_snapshots VALUES (?1,?2,?3,?4,?5,?6,1)",
                        params![
                            project.to_string(),
                            repository.to_string(),
                            workspace.to_string(),
                            generation,
                            format!("legacy-snapshot-{workspace}"),
                            format!("legacy-graph-{workspace}")
                        ],
                    )
                    .unwrap();
                connection
                    .execute(
                        "INSERT INTO cg_workspace_heads VALUES (?1,?2)",
                        params![workspace.to_string(), generation],
                    )
                    .unwrap();
                let old_file_id = Uuid::new_v5(
                    &WORKSPACE_NS,
                    format!("{workspace}\0src/lib.rs").as_bytes(),
                );
                connection
                    .execute(
                        "INSERT INTO cg_files VALUES (?1,?2,'src/lib.rs',?3,?4,?5)",
                        params![
                            workspace.to_string(),
                            generation,
                            old_file_id.to_string(),
                            source,
                            source_digest
                        ],
                    )
                    .unwrap();
                for (key, kind, name, node_span) in &node_facts {
                    connection
                        .execute(
                            "INSERT INTO cg_nodes VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                            params![
                                workspace.to_string(),
                                generation,
                                ids[key].to_string(),
                                key,
                                kind.as_str(),
                                name,
                                node_span.path,
                                node_span.start_byte,
                                node_span.end_byte,
                                node_span.start_line,
                                node_span.start_column,
                                node_span.end_line,
                                node_span.end_column
                            ],
                        )
                        .unwrap();
                }
                let relations = [
                    (
                        "owns-alpha",
                        "Owns",
                        "sample",
                        "sample::alpha",
                        vec![EvidenceFact {
                            label: "legacy ownership evidence".into(),
                            span: span("src/lib.rs", "alpha", 0),
                        }],
                    ),
                    (
                        "calls-beta",
                        "Calls",
                        "sample::alpha",
                        "sample::beta",
                        vec![
                            EvidenceFact {
                                label: "legacy call evidence".into(),
                                span: span("src/lib.rs", "beta", 0),
                            },
                            EvidenceFact {
                                label: "legacy call evidence".into(),
                                span: span("src/lib.rs", "beta", 1),
                            },
                        ],
                    ),
                ];
                for (key, kind, source_key, target_key, evidence_sites) in relations {
                    let source_id = ids[source_key];
                    let target_id = ids[target_key];
                    let relation_uuid = Uuid::new_v5(
                        &RELATION_NS,
                        format!(
                            "{repository}\0{kind}\0{source_id}\0{target_id}\0{key}"
                        )
                        .as_bytes(),
                    );
                    connection
                        .execute(
                            "INSERT INTO cg_relations VALUES (?1,?2,?3,?4,?5,?6,?7)",
                            params![
                                workspace.to_string(),
                                generation,
                                relation_uuid.to_string(),
                                key,
                                kind,
                                source_id.to_string(),
                                target_id.to_string()
                            ],
                        )
                        .unwrap();
                    for (ordinal, evidence) in evidence_sites.iter().enumerate() {
                        let start = &evidence.span;
                        let evidence_digest = evidence_digest(&source_digest, evidence);
                        connection
                            .execute(
                                "INSERT INTO cg_evidence VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                                params![
                                    workspace.to_string(),
                                    generation,
                                    relation_uuid.to_string(),
                                    ordinal,
                                    evidence.label,
                                    start.path,
                                    start.start_byte,
                                    start.end_byte,
                                    start.start_line,
                                    start.start_column,
                                    start.end_line,
                                    start.end_column,
                                    source_digest,
                                    evidence_digest
                                ],
                            )
                            .unwrap();
                    }
                }
                (workspace.to_owned(), generation)
            })
            .collect::<std::collections::HashMap<_, _>>();
        workspaces
            .iter()
            .map(|workspace| generation_by_workspace[workspace])
            .collect()
    }

    fn expected_migrated_v1_bundle() -> SourceFactBundle {
        let mut bundle = fixture();
        bundle.extraction.extractor = ExtractorIdentity {
            name: "legacy-v1-migration".into(),
            version: "1".into(),
        };
        for node in &mut bundle.nodes {
            node.identity = NodeIdentity {
                language: "legacy-v1".into(),
                qualified_name: node.key.0.clone(),
                disambiguator: "v1".into(),
            };
            node.evidence = vec![EvidenceFact {
                label: "legacy source-backed node".into(),
                span: node.span.clone(),
            }];
        }
        bundle.relations = vec![
            RelationFact {
                key: FactKey("owns-alpha".into()),
                owner_file: "src/lib.rs".into(),
                site_anchor: "legacy-v1:owns-alpha".into(),
                kind: RelationKind::Contains,
                source: FactKey("sample".into()),
                target: FactKey("sample::alpha".into()),
                provenance: FactProvenance::Extracted,
                evidence: vec![EvidenceFact {
                    label: "legacy ownership evidence".into(),
                    span: span("src/lib.rs", "alpha", 0),
                }],
            },
            RelationFact {
                key: FactKey("calls-beta".into()),
                owner_file: "src/lib.rs".into(),
                site_anchor: "legacy-v1:calls-beta".into(),
                kind: RelationKind::Calls,
                source: FactKey("sample::alpha".into()),
                target: FactKey("sample::beta".into()),
                provenance: FactProvenance::Extracted,
                evidence: vec![
                    EvidenceFact {
                        label: "legacy call evidence".into(),
                        span: span("src/lib.rs", "beta", 0),
                    },
                    EvidenceFact {
                        label: "legacy call evidence".into(),
                        span: span("src/lib.rs", "beta", 1),
                    },
                ],
            },
        ];
        bundle.unresolved_references.clear();
        bundle
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One reopened fixture checks every legacy ready projection.
    fn populated_v1_migration_preserves_ready_facts_and_upgrades_v2_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy-codegraph.sqlite");
        let project = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let primary = CodegraphStore::workspace_id(project, "primary").unwrap();
        let worktree = CodegraphStore::workspace_id(project, "worktree:legacy").unwrap();
        let workspaces = [primary, worktree];
        let generations = create_populated_v1_database(&path, project, &workspaces);
        let expected_bundle = expected_migrated_v1_bundle();
        let legacy = Connection::open(&path).unwrap();
        for workspace in workspaces {
            let stored: String = legacy
                .query_row(
                    "SELECT file_id FROM cg_files WHERE workspace_id=?1 AND path='src/lib.rs'",
                    [workspace.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let expected =
                Uuid::new_v5(&WORKSPACE_NS, format!("{workspace}\0src/lib.rs").as_bytes());
            assert_eq!(parse_uuid(&stored).unwrap(), expected);
        }
        drop(legacy);
        let store = CodegraphStore::open(&path, project).unwrap();

        let mut migrated_file_ids = Vec::new();
        for (workspace, generation) in workspaces.into_iter().zip(generations) {
            let ready = store.current_ready(workspace).unwrap();
            assert_eq!(ready.generation, generation);
            assert_eq!(ready.extraction.mode, ExtractionMode::ExtractedV1_0);
            assert_eq!(ready.extraction.extractor.name, "legacy-v1-migration");
            assert_eq!(ready.extraction.extractor.version, "1");
            assert!(ready.snapshot_digest != format!("legacy-snapshot-{workspace}"));
            assert!(ready.graph_digest != format!("legacy-graph-{workspace}"));
            assert_eq!(
                ready.snapshot_digest,
                bundle_digest(
                    &expected_bundle,
                    project,
                    CodegraphStore::repository_id(project),
                    workspace
                )
            );
            assert_eq!(
                ready.graph_digest,
                graph_digest(
                    &expected_bundle,
                    project,
                    CodegraphStore::repository_id(project),
                    workspace
                )
            );

            let nodes = store.search_path(workspace, "src/lib.rs", 10).unwrap();
            assert_eq!(nodes.len(), 3);
            assert!(
                nodes
                    .iter()
                    .all(|node| node.provenance == FactProvenance::Extracted)
            );
            assert!(nodes.iter().all(|node| node.evidence.len() == 1));
            assert!(nodes.iter().all(|node| {
                let evidence = &node.evidence[0];
                evidence.label == "legacy source-backed node"
                    && evidence.source_digest
                        == blake3::hash(SOURCE.as_bytes()).to_hex().to_string()
                    && !evidence.evidence_digest.is_empty()
                    && SOURCE.as_bytes()[evidence.span.start_byte..evidence.span.end_byte]
                        == SOURCE.as_bytes()[node.span.start_byte..node.span.end_byte]
            }));
            let alpha = store.search_name(workspace, "alpha", 1).unwrap();
            assert_eq!(alpha.len(), 1);
            let explanation = store.explain(workspace, alpha[0].id).unwrap();
            assert_eq!(explanation.node.evidence.len(), 1);
            assert_eq!(explanation.ownership.len(), 1);
            assert_eq!(explanation.ownership[0].kind, RelationKind::Contains);
            assert_eq!(
                explanation.ownership[0].provenance,
                FactProvenance::Extracted
            );
            assert_ne!(explanation.ownership[0].source, explanation.node.id);
            assert_eq!(explanation.ownership[0].target, explanation.node.id);
            assert_eq!(explanation.ownership[0].evidence.len(), 1);
            assert_eq!(explanation.directed_relations.len(), 1);
            assert_eq!(explanation.directed_relations[0].kind, RelationKind::Calls);
            assert_eq!(explanation.directed_relations[0].evidence.len(), 2);
            assert_ne!(
                explanation.directed_relations[0].evidence[0]
                    .span
                    .start_byte,
                explanation.directed_relations[0].evidence[1]
                    .span
                    .start_byte,
            );
            assert!(
                explanation
                    .ownership
                    .iter()
                    .chain(&explanation.directed_relations)
                    .flat_map(|relation| &relation.evidence)
                    .all(|evidence| evidence.source_digest
                        == blake3::hash(SOURCE.as_bytes()).to_hex().to_string())
            );
            migrated_file_ids.push(store.file_id(workspace, "src/lib.rs").unwrap());
        }
        assert_eq!(migrated_file_ids[0], migrated_file_ids[1]);

        let columns = store
            .connection
            .prepare("PRAGMA table_info(cg_files)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(!columns.iter().any(|column| column == "bytes"));
        let ready_heads = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM cg_workspace_heads h JOIN cg_snapshots s
                   ON s.workspace_id=h.workspace_id AND s.generation=h.generation
                 WHERE s.ready=1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(ready_heads, 2);
    }

    #[test]
    fn populated_v5_migration_preserves_ready_status_and_v6_metrics_survive_restart() {
        use crate::lifecycle::{IndexLifecyclePhase, IndexRunMetrics};

        let (directory, mut store, project, workspace) = store();
        let path = directory.path().join("codegraph.sqlite");
        let ready = publish_primary(&mut store, &fixture()).unwrap();
        let source_digest = blake3::hash(SOURCE.as_bytes()).to_hex().to_string();
        let legacy_at = "2026-09-23T00:00:00.000000000Z";
        store
            .connection
            .execute(
                "INSERT INTO cg_index_state(workspace_id,phase,source_digest,last_success_at,
                files_discovered,bytes_discovered,updated_at)
             VALUES (?1,'Ready',?2,?3,1,?4,?3)",
                params![
                    workspace.to_string(),
                    source_digest,
                    legacy_at,
                    SOURCE.len()
                ],
            )
            .unwrap();
        store.connection.execute(
            "INSERT INTO cg_index_manifest(workspace_id,path,source_digest) VALUES (?1,'src/lib.rs',?2)",
            params![workspace.to_string(), source_digest],
        ).unwrap();
        let expected_counts = store.index_status(workspace).unwrap().unwrap().ready_counts;
        let expected_alpha = store.search_name(workspace, "alpha", 1).unwrap();
        let expected_manifest = store.index_manifest(workspace).unwrap();
        assert!(
            expected_counts
                .as_ref()
                .is_some_and(|counts| counts.nodes > 0 && counts.relations > 0)
        );
        drop(store);

        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "ALTER TABLE cg_index_state DROP COLUMN files_hashed;
             ALTER TABLE cg_index_state DROP COLUMN files_reused;
             ALTER TABLE cg_index_state DROP COLUMN files_extracted;
             ALTER TABLE cg_index_state DROP COLUMN rescan_reason;
             PRAGMA user_version = 5;",
            )
            .unwrap();
        let version: i64 = legacy
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 5);
        drop(legacy);

        let mut migrated = CodegraphStore::open(&path, project).unwrap();
        let status = migrated.index_status(workspace).unwrap().unwrap();
        assert_eq!(status.schema_version, 6);
        assert_eq!(status.phase, IndexLifecyclePhase::Ready);
        assert_eq!(status.ready, Some(ready.clone()));
        assert_eq!(status.ready_counts, expected_counts);
        assert_eq!(status.last_success_at.as_deref(), Some(legacy_at));
        assert_eq!(status.metrics.files_discovered, 1);
        assert_eq!(status.metrics.bytes_discovered, SOURCE.len());
        assert_eq!(status.metrics.files_hashed, 0);
        assert_eq!(status.metrics.files_reused, 0);
        assert_eq!(status.metrics.files_extracted, 0);
        assert_eq!(status.rescan_reason, None);
        assert_eq!(
            migrated.search_name(workspace, "alpha", 1).unwrap(),
            expected_alpha
        );
        assert_eq!(
            migrated.index_manifest(workspace).unwrap(),
            expected_manifest
        );

        let extraction = StagedExtraction {
            extraction: fixture().extraction,
            grammar_version: "tree-sitter-test".into(),
            rule_version: "test".into(),
            normalization_version: "test".into(),
            config_digest: "a".repeat(64),
        };
        let run = migrated
            .begin_index_run(workspace, &source_digest, &extraction)
            .unwrap();
        let metrics = IndexRunMetrics {
            files_discovered: 1,
            bytes_discovered: SOURCE.len(),
            files_hashed: 1,
            files_reused: 1,
            files_extracted: 0,
            changed_paths: 0,
            staged_files: 1,
            duration_ms: 17,
            overflow_count: 2,
            pending_rescan: true,
            rescan_reason: Some("watcher_overflow".into()),
        };
        migrated
            .finish_index_run(
                run,
                workspace,
                IndexLifecyclePhase::Stale,
                &metrics,
                None,
                None,
            )
            .unwrap();
        drop(migrated);

        let reopened = CodegraphStore::open(&path, project).unwrap();
        let status = reopened.index_status(workspace).unwrap().unwrap();
        assert_eq!(status.phase, IndexLifecyclePhase::Stale);
        assert_eq!(status.ready, Some(ready));
        assert_eq!(status.ready_counts, expected_counts);
        assert_eq!(status.metrics, metrics);
        assert_eq!(status.rescan_reason.as_deref(), Some("watcher_overflow"));
        assert_eq!(
            reopened.index_manifest(workspace).unwrap(),
            expected_manifest
        );
    }

    #[test]
    fn failed_v1_migration_rolls_back_schema_and_keeps_legacy_ready_head() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid-legacy-codegraph.sqlite");
        let project = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let workspace = CodegraphStore::workspace_id(project, "primary").unwrap();
        let generation = create_populated_v1_database(&path, project, &[workspace])[0];
        let legacy_file_id =
            Uuid::new_v5(&WORKSPACE_NS, format!("{workspace}\0src/lib.rs").as_bytes());
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute(
                "UPDATE cg_nodes SET kind='Unknown' WHERE workspace_id=?1 AND node_key='sample'",
                [workspace.to_string()],
            )
            .unwrap();
        drop(legacy);

        assert!(matches!(
            CodegraphStore::open(&path, project),
            Err(CodegraphError::Sqlite(_))
        ));
        let legacy = Connection::open(&path).unwrap();
        let version: i64 = legacy
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let file_columns = legacy
            .prepare("PRAGMA table_info(cg_files)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(file_columns.iter().any(|column| column == "bytes"));
        let file_id: String = legacy
            .query_row(
                "SELECT file_id FROM cg_files WHERE workspace_id=?1 AND generation=?2",
                params![workspace.to_string(), generation],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parse_uuid(&file_id).unwrap(), legacy_file_id);
        let relation_kind: String = legacy
            .query_row(
                "SELECT kind FROM cg_relations
                 WHERE workspace_id=?1 AND generation=?2 AND relation_key='owns-alpha'",
                params![workspace.to_string(), generation],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(relation_kind, "Owns");
        let ready: i64 = legacy
            .query_row(
                "SELECT s.ready FROM cg_workspace_heads h JOIN cg_snapshots s
                   ON s.workspace_id=h.workspace_id AND s.generation=h.generation
                 WHERE h.workspace_id=?1",
                [workspace.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ready, 1);
    }

    #[test]
    fn search_and_explain_preserve_node_evidence_direction_and_unresolved_sites() {
        let (_directory, mut store, _project, workspace) = store();
        publish_primary(&mut store, &fixture()).unwrap();
        let result = store.search_name(workspace, "alpha", 10).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            &SOURCE.as_bytes()[result[0].span.start_byte..result[0].span.end_byte],
            b"alpha"
        );
        assert_eq!(
            result[0].evidence.len(),
            2,
            "same-labelled node evidence remains distinct"
        );
        assert!(result[0].evidence.iter().all(|item| {
            item.label == "syntax declaration"
                && item.source_digest == blake3::hash(SOURCE.as_bytes()).to_hex().to_string()
                && !item.evidence_digest.is_empty()
        }));
        assert!(result[0].evidence.iter().any(|item| {
            &SOURCE.as_bytes()[item.span.start_byte..item.span.end_byte] == b"alpha"
        }));
        assert_eq!(
            store
                .search_path(workspace, "src/lib.rs", 10)
                .unwrap()
                .len(),
            3
        );

        let view = store.explain(workspace, result[0].id).unwrap();
        assert_eq!(view.node.evidence.len(), 2);
        assert_eq!(view.ownership.len(), 1);
        assert_eq!(
            view.ownership[0].target,
            store.search_name(workspace, "alpha", 1).unwrap()[0].id
        );
        assert_ne!(view.ownership[0].source, view.node.id);
        assert_eq!(
            view.directed_relations.len(),
            2,
            "parallel directed call facts remain separate"
        );
        assert!(view.directed_relations.iter().all(
            |relation| relation.source == view.node.id && relation.kind == RelationKind::Calls
        ));
        let evidence = view
            .directed_relations
            .iter()
            .flat_map(|relation| &relation.evidence)
            .collect::<Vec<_>>();
        assert_eq!(evidence.len(), 3);
        assert!(evidence.iter().all(|item| item.label == "call expression"));
        assert!(evidence.iter().all(|item| {
            &SOURCE.as_bytes()[item.span.start_byte..item.span.end_byte] == b"beta"
        }));
        assert_eq!(
            evidence
                .iter()
                .map(|item| item.span.start_byte)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
        assert!(
            evidence
                .iter()
                .all(|item| item.source_digest
                    == blake3::hash(SOURCE.as_bytes()).to_hex().to_string())
        );
        assert_eq!(view.unresolved_references.len(), 1);
        let unresolved = &view.unresolved_references[0];
        assert_eq!(unresolved.kind, UnresolvedReferenceKind::Call);
        assert_eq!(unresolved.owner, view.node.id);
        assert_eq!(unresolved.raw_target, "missing_helper");
        assert_eq!(
            &SOURCE.as_bytes()[unresolved.span.start_byte..unresolved.span.end_byte],
            b"missing_helper"
        );
    }

    #[test]
    fn canonical_digest_is_independent_of_input_order() {
        let mut first = fixture();
        let mut second = fixture();
        second.files.reverse();
        second.nodes.reverse();
        second.relations.reverse();
        second.relations[1].evidence.reverse();
        let (_directory, mut store, _project, workspace) = store();
        let first_ready = publish_primary(&mut store, &first).unwrap();
        let second_ready = publish_primary(&mut store, &second).unwrap();
        assert_eq!(first_ready.snapshot_digest, second_ready.snapshot_digest);
        assert_eq!(first_ready.graph_digest, second_ready.graph_digest);
        assert_eq!(first_ready.generation, second_ready.generation);
        first.nodes[0].name.push_str(" changed");
        assert_ne!(
            bundle_digest(&first, Uuid::nil(), Uuid::nil(), workspace),
            bundle_digest(&second, Uuid::nil(), Uuid::nil(), workspace)
        );
    }

    #[test]
    fn clean_rebuilds_with_reordered_files_and_facts_have_identical_digests() {
        let mut first = fixture();
        first.files.push(SourceFile {
            relative_path: "README.md".into(),
            bytes: b"# Sample\n".to_vec(),
        });
        let mut reordered = first.clone();
        reordered.files.reverse();
        reordered.nodes.reverse();
        reordered.relations.reverse();
        reordered.nodes[1].evidence.reverse();
        let project = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let left_dir = tempfile::tempdir().unwrap();
        let right_dir = tempfile::tempdir().unwrap();
        let mut left = CodegraphStore::open(left_dir.path().join("graph.sqlite"), project).unwrap();
        let mut right =
            CodegraphStore::open(right_dir.path().join("graph.sqlite"), project).unwrap();
        let left_ready = publish_primary(&mut left, &first).unwrap();
        let right_ready = publish_primary(&mut right, &reordered).unwrap();
        assert_eq!(left_ready.snapshot_digest, right_ready.snapshot_digest);
        assert_eq!(left_ready.graph_digest, right_ready.graph_digest);
        let left_nodes = left
            .search_name(left_ready.workspace_id, "alpha", 1)
            .unwrap();
        let right_nodes = right
            .search_name(right_ready.workspace_id, "alpha", 1)
            .unwrap();
        assert_eq!(left_nodes.nodes, right_nodes.nodes);
    }

    #[test]
    fn graph_digest_tracks_source_hash_changes_at_unchanged_spans() {
        let original = fixture();
        let mut changed = original.clone();
        changed.files[0].bytes[0] = b'n';
        assert_ne!(
            graph_digest(&original, Uuid::nil(), Uuid::nil(), Uuid::nil()),
            graph_digest(&changed, Uuid::nil(), Uuid::nil(), Uuid::nil())
        );
    }

    #[test]
    fn file_ids_are_repository_relative_and_shared_across_workspaces() {
        let (_directory, mut store, _project, primary) = store();
        let scope = store.scope(WorkspaceInstanceKey::GitWorktree(Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            b"stable-id",
        )));
        let worktree = scope.workspace_id();
        publish_primary(&mut store, &fixture()).unwrap();
        store.publish(&scope, &fixture()).unwrap();
        assert_eq!(
            store.file_id(primary, "src/lib.rs").unwrap(),
            store.file_id(worktree, "src/lib.rs").unwrap()
        );
        assert_eq!(store.current_ready(primary).unwrap().generation, 1);
        assert_eq!(store.current_ready(worktree).unwrap().generation, 1);
    }

    #[test]
    fn failed_publish_keeps_previous_ready_head_queryable() {
        let (_directory, mut store, _project, workspace) = store();
        let before = publish_primary(&mut store, &fixture()).unwrap();
        let mut changed = fixture();
        changed.files[0].bytes.extend_from_slice(b"// next\n");
        let scope = store.scope(WorkspaceInstanceKey::Primary);
        assert!(matches!(
            store.publish_with_fault(&scope, &changed, PublishFault::BeforeHeadFlip),
            Err(CodegraphError::InjectedPublishFailure)
        ));
        let after = store.current_ready(workspace).unwrap();
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.snapshot_digest, before.snapshot_digest);
        assert_eq!(store.search_name(workspace, "alpha", 2).unwrap().len(), 1);
    }

    #[test]
    fn same_content_reappearing_after_another_ready_snapshot_advances_generation() {
        let (_directory, mut store, _project, _workspace) = store();
        let original = fixture();
        let first = publish_primary(&mut store, &original).unwrap();
        let mut changed = original.clone();
        changed.files[0].bytes.extend_from_slice(b"// changed\n");
        let second = publish_primary(&mut store, &changed).unwrap();
        let reverted = publish_primary(&mut store, &original).unwrap();
        assert_eq!(second.generation, first.generation + 1);
        assert_eq!(reverted.generation, second.generation + 1);
        assert_eq!(reverted.snapshot_digest, first.snapshot_digest);
    }

    #[test]
    fn database_is_bound_to_one_project() {
        let (directory, store, _project, _workspace) = store();
        let mut table_info = store
            .connection
            .prepare("PRAGMA table_info(cg_files)")
            .unwrap();
        let columns = table_info
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "source_digest"));
        assert!(!columns.iter().any(|column| column == "bytes"));
        let other_project = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let reopened =
            CodegraphStore::open(directory.path().join("codegraph.sqlite"), other_project);
        assert!(matches!(reopened, Err(CodegraphError::InvalidInput(_))));
    }

    #[test]
    fn rejects_invalid_spans_paths_missing_endpoints_and_output_bounds() {
        let (_directory, mut store, _project, workspace) = store();
        let mut invalid_span = fixture();
        invalid_span.nodes[0].span.end_byte += 1;
        assert!(matches!(
            publish_primary(&mut store, &invalid_span),
            Err(CodegraphError::InvalidInput(_))
        ));
        let mut invalid_path = fixture();
        invalid_path.files[0].relative_path = "../escape.rs".into();
        assert!(matches!(
            publish_primary(&mut store, &invalid_path),
            Err(CodegraphError::InvalidInput(_))
        ));
        let mut missing_endpoint = fixture();
        missing_endpoint.relations[0].target = FactKey("missing".into());
        assert!(matches!(
            publish_primary(&mut store, &missing_endpoint),
            Err(CodegraphError::MissingNode(_))
        ));
        let mut oversized_file = fixture();
        oversized_file.files[0]
            .bytes
            .resize(MAX_FILE_BYTES + 1, b'x');
        assert!(matches!(
            publish_primary(&mut store, &oversized_file),
            Err(CodegraphError::LimitExceeded { .. })
        ));
        publish_primary(&mut store, &fixture()).unwrap();
        assert!(matches!(
            store.search_name(workspace, "alpha", MAX_QUERY_LIMIT + 1),
            Err(CodegraphError::LimitExceeded { .. })
        ));
        assert!(matches!(
            store.search_name(workspace, "", 1),
            Err(CodegraphError::InvalidInput(_))
        ));
    }

    #[test]
    fn publish_rejects_foreign_scope_and_non_extracted_facts() {
        let (_directory, mut store, _project, workspace) = store();
        let mut foreign = store.scope(WorkspaceInstanceKey::Primary);
        foreign.project_id = Uuid::new_v4();
        assert!(matches!(
            store.publish(&foreign, &fixture()),
            Err(CodegraphError::InvalidInput(_))
        ));
        assert!(matches!(
            store.current_ready(workspace),
            Err(CodegraphError::NoReadySnapshot)
        ));

        let mut bundle = fixture();
        bundle.extraction.mode = ExtractionMode::Exploratory;
        assert!(matches!(
            publish_primary(&mut store, &bundle),
            Err(CodegraphError::InvalidInput(_))
        ));
        bundle = fixture();
        bundle.extraction.extractor.version.clear();
        assert!(matches!(
            publish_primary(&mut store, &bundle),
            Err(CodegraphError::InvalidInput(_))
        ));
        bundle = fixture();
        bundle.nodes[0].provenance = FactProvenance::Inferred;
        assert!(matches!(
            publish_primary(&mut store, &bundle),
            Err(CodegraphError::InvalidInput(_))
        ));
        bundle = fixture();
        bundle.relations[0].provenance = FactProvenance::Ambiguous;
        assert!(matches!(
            publish_primary(&mut store, &bundle),
            Err(CodegraphError::InvalidInput(_))
        ));
        bundle = fixture();
        bundle.nodes[0].identity.language.clear();
        assert!(matches!(
            publish_primary(&mut store, &bundle),
            Err(CodegraphError::InvalidInput(_))
        ));
        bundle = fixture();
        bundle.relations[0].owner_file = "src/other.rs".into();
        assert!(matches!(
            publish_primary(&mut store, &bundle),
            Err(CodegraphError::InvalidInput(_))
        ));
        bundle = fixture();
        bundle.relations[0].site_anchor.clear();
        assert!(matches!(
            publish_primary(&mut store, &bundle),
            Err(CodegraphError::InvalidInput(_))
        ));
        bundle = fixture();
        bundle.relations[0].site_anchor = "raw-line-12".into();
        assert!(matches!(
            publish_primary(&mut store, &bundle),
            Err(CodegraphError::InvalidInput(_))
        ));
        let ready = publish_primary(&mut store, &fixture()).unwrap();
        assert_eq!(ready.extraction.mode, ExtractionMode::ExtractedV1_0);
        assert_eq!(
            ready.extraction.extractor.name,
            "hand-authored-rust-fixture"
        );
        assert_eq!(
            store.search_name(workspace, "alpha", 1).unwrap()[0].provenance,
            FactProvenance::Extracted
        );
    }

    #[test]
    fn identity_and_digest_domains_include_source_and_extractor_inputs() {
        let bundle = fixture();
        let project = Uuid::new_v4();
        let repository = CodegraphStore::repository_id(project);
        let workspace = Uuid::new_v4();
        let node = &bundle.nodes[0];
        let original_id = node_id(repository, node);
        let mut moved = node.clone();
        moved.span.path = "src/moved.rs".into();
        assert_ne!(original_id, node_id(repository, &moved));
        let mut other_language = node.clone();
        other_language.identity.language = "markdown".into();
        assert_ne!(original_id, node_id(repository, &other_language));
        let relation = &bundle.relations[2];
        let source = node_id(repository, &bundle.nodes[1]);
        let target = node_id(repository, &bundle.nodes[2]);
        let original_relation = relation_id(
            repository,
            relation.kind,
            source,
            target,
            &relation.owner_file,
            &relation.site_anchor,
        );
        assert_ne!(
            original_relation,
            relation_id(
                repository,
                relation.kind,
                source,
                target,
                "src/other.rs",
                &relation.site_anchor
            )
        );
        assert_ne!(
            original_relation,
            relation_id(
                repository,
                relation.kind,
                source,
                target,
                &relation.owner_file,
                "other-site"
            )
        );

        let digest = graph_digest(&bundle, project, repository, workspace);
        assert_ne!(
            digest,
            graph_digest(&bundle, Uuid::new_v4(), repository, workspace)
        );
        assert_ne!(
            digest,
            graph_digest(&bundle, project, Uuid::new_v4(), workspace)
        );
        let mut changed = bundle.clone();
        changed.extraction.extractor.version = "2.0.0".into();
        assert_ne!(
            digest,
            graph_digest(&changed, project, repository, workspace)
        );
        changed = bundle.clone();
        changed.relations[0].provenance = FactProvenance::Inferred;
        assert_ne!(
            digest,
            graph_digest(&changed, project, repository, workspace)
        );
    }

    #[test]
    fn search_reports_completeness_and_evidence_order_is_worker_independent() {
        let (_directory, mut store, _project, primary) = store();
        let mut first = fixture();
        let duplicate = first.nodes[1].evidence[0].clone();
        first.nodes[1].evidence.push(duplicate);
        publish_primary(&mut store, &first).unwrap();
        let partial = store.search_path(primary, "src/lib.rs", 1).unwrap();
        assert_eq!(partial.requested_limit, 1);
        assert_eq!(partial.effective_limit, 1);
        assert!(!partial.complete);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial.snapshot.workspace_id, primary);
        let complete = store.search_name(primary, "alpha", 2).unwrap();
        assert!(complete.complete);
        assert_eq!(complete.len(), 1);

        let scope = store.scope(WorkspaceInstanceKey::GitWorktree(Uuid::new_v4()));
        let mut reversed = first.clone();
        reversed.nodes[1].evidence.reverse();
        reversed.relations[2].evidence.reverse();
        store.publish(&scope, &reversed).unwrap();
        let primary_node = &complete[0];
        let worktree_node = &store.search_name(scope.workspace_id(), "alpha", 2).unwrap()[0];
        assert_eq!(primary_node.evidence, worktree_node.evidence);
        assert_eq!(primary_node.evidence.len(), 3);
        assert_eq!(
            primary_node
                .evidence
                .iter()
                .filter(|item| item.span == span("src/lib.rs", "alpha", 0))
                .count(),
            2
        );
        let primary_explain = store.explain(primary, primary_node.id).unwrap();
        let worktree_explain = store
            .explain(scope.workspace_id(), worktree_node.id)
            .unwrap();
        assert_eq!(
            primary_explain.directed_relations,
            worktree_explain.directed_relations
        );
        assert_eq!(
            primary_explain
                .directed_relations
                .iter()
                .flat_map(|r| &r.evidence)
                .count(),
            3
        );
    }

    #[test]
    fn explain_fails_visibly_when_relation_bound_is_exceeded() {
        let (_directory, mut store, _project, workspace) = store();
        let mut bundle = fixture();
        let template = bundle.relations[2].clone();
        bundle.relations.clear();
        for index in 0..=MAX_EXPLAIN_RELATIONS {
            let mut relation = template.clone();
            relation.key = FactKey(format!("parallel-{index}"));
            relation.site_anchor = format!("v1:alpha/call-{index}");
            bundle.relations.push(relation);
        }
        publish_primary(&mut store, &bundle).unwrap();
        let alpha = store.search_name(workspace, "alpha", 1).unwrap()[0].id;
        assert!(matches!(
            store.explain(workspace, alpha),
            Err(CodegraphError::LimitExceeded { requested, maximum })
                if requested == MAX_EXPLAIN_RELATIONS + 1 && maximum == MAX_EXPLAIN_RELATIONS
        ));
    }
}
