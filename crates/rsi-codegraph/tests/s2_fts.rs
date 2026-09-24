#![allow(clippy::expect_used, clippy::unwrap_used)] // Exact temporary database fixtures.

use rsi_codegraph::{
    CodegraphStore, EvidenceFact, ExtractionContract, ExtractionMode, ExtractorIdentity, FactKey,
    FactProvenance, FileDiagnostic, NodeFact, NodeIdentity, NodeKind, PerFileFacts, PublishFault,
    SourceFile, SourceSpan, SourceVersion, StagedExtraction, WorkspaceInstanceKey,
    query::{QueryFilter, QueryLimits, SearchMode, SnapshotSelector},
};
use rusqlite::{Connection, params};
use uuid::Uuid;

const PROJECT: Uuid = Uuid::from_u128(0xcdcdcdcd_cdcd_4dcd_8dcd_cdcdcdcdcdcd);

fn facts(path: &str, name: &str) -> PerFileFacts {
    let file = SourceFile {
        relative_path: path.into(),
        bytes: b"node".to_vec(),
    };
    let span = SourceSpan {
        path: path.into(),
        start_byte: 0,
        end_byte: 4,
        start_line: 1,
        start_column: 1,
        end_line: 1,
        end_column: 5,
    };
    PerFileFacts {
        file,
        nodes: vec![NodeFact {
            key: FactKey(format!("node:{path}")),
            identity: NodeIdentity {
                language: "rust".into(),
                qualified_name: format!("fixture::{path}"),
                disambiguator: "v1:node".into(),
            },
            kind: NodeKind::Function,
            name: name.into(),
            span: span.clone(),
            provenance: FactProvenance::Extracted,
            evidence: vec![EvidenceFact {
                label: "source".into(),
                span,
            }],
        }],
        relations: vec![],
        unresolved_references: vec![],
        diagnostic: FileDiagnostic::Parsed,
    }
}
fn manifest() -> StagedExtraction {
    StagedExtraction {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "s2-fts".into(),
                version: "1".into(),
            },
        },
        grammar_version: "rust-1".into(),
        rule_version: "rules-1".into(),
        normalization_version: "norm-1".into(),
        config_digest: "config-1".into(),
    }
}
fn version(facts: &PerFileFacts) -> SourceVersion {
    SourceVersion {
        relative_path: facts.file.relative_path.clone(),
        source_digest: blake3::hash(&facts.file.bytes).to_hex().to_string(),
    }
}
fn fts(
    store: &CodegraphStore,
    workspace: Uuid,
    selector: SnapshotSelector,
    term: &str,
) -> Vec<String> {
    store
        .query(workspace, selector)
        .unwrap()
        .search(
            SearchMode::Fts,
            term,
            &QueryFilter::default(),
            QueryLimits::default(),
        )
        .unwrap()
        .value
        .into_iter()
        .map(|node| node.name)
        .collect()
}

#[test]
fn staged_publish_and_delete_keep_fts_source_owned_per_generation() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = CodegraphStore::open(&database, PROJECT).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let alpha = facts("src/alpha.rs", "AlphaHandler");
    let beta = facts("src/beta.rs", "BetaHandler");
    let first = store
        .begin_staged(&scope, &manifest(), &[version(&alpha), version(&beta)])
        .unwrap();
    store.stage_file(&first, &alpha).unwrap();
    store.stage_file(&first, &beta).unwrap();
    let old = store.publish_staged(&first, PublishFault::None).unwrap();
    assert_eq!(
        fts(
            &store,
            scope.workspace_id(),
            SnapshotSelector::CurrentReady,
            "BetaHandler"
        ),
        vec!["BetaHandler"]
    );
    let second = store
        .begin_staged(&scope, &manifest(), &[version(&alpha)])
        .unwrap();
    store.stage_file(&second, &alpha).unwrap();
    assert!(
        store
            .publish_staged(&second, PublishFault::BeforeHeadFlip)
            .is_err()
    );
    assert_eq!(
        store
            .current_ready(scope.workspace_id())
            .unwrap()
            .generation,
        old.generation
    );
    assert_eq!(
        fts(
            &store,
            scope.workspace_id(),
            SnapshotSelector::CurrentReady,
            "BetaHandler"
        ),
        vec!["BetaHandler"]
    );
    let current = store.publish_staged(&second, PublishFault::None).unwrap();
    assert_eq!(current.generation, old.generation + 1);
    assert!(
        fts(
            &store,
            scope.workspace_id(),
            SnapshotSelector::CurrentReady,
            "BetaHandler"
        )
        .is_empty()
    );
    assert_eq!(
        fts(
            &store,
            scope.workspace_id(),
            SnapshotSelector::Generation(old.generation),
            "BetaHandler"
        ),
        vec!["BetaHandler"]
    );
    let conn = Connection::open(&database).unwrap();
    let current_beta_rows: i64 = conn.query_row("SELECT count(*) FROM cg_fts_nodes WHERE workspace_id=?1 AND generation=?2 AND owner_path='src/beta.rs'", params![scope.workspace_id().to_string(),current.generation], |row| row.get(0)).unwrap();
    assert_eq!(current_beta_rows, 0);
    let current_alpha_rows: i64 = conn.query_row("SELECT count(*) FROM cg_fts_nodes WHERE workspace_id=?1 AND generation=?2 AND owner_path='src/alpha.rs'", params![scope.workspace_id().to_string(),current.generation], |row| row.get(0)).unwrap();
    assert_eq!(current_alpha_rows, 1);
}

#[test]
fn forward_v4_migration_backfills_ready_v3_nodes_without_rewriting_history() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = CodegraphStore::open(&database, PROJECT).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let alpha = facts("src/alpha.rs", "AlphaHandler");
    let run = store
        .begin_staged(&scope, &manifest(), &[version(&alpha)])
        .unwrap();
    store.stage_file(&run, &alpha).unwrap();
    let ready = store.publish_staged(&run, PublishFault::None).unwrap();
    drop(store);
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "DROP TABLE cg_retained_relations;
        DROP TABLE cg_retained_nodes;
        DROP TABLE cg_generation_detail;
        DROP TABLE cg_index_manifest;
        DROP TABLE cg_index_runs;
        DROP TABLE cg_index_state;
        DROP TABLE cg_fts_nodes;
        PRAGMA user_version = 3;",
    )
    .unwrap();
    drop(conn);
    let reopened = CodegraphStore::open(&database, PROJECT).unwrap();
    assert_eq!(
        fts(
            &reopened,
            scope.workspace_id(),
            SnapshotSelector::Generation(ready.generation),
            "AlphaHandler"
        ),
        vec!["AlphaHandler"]
    );
    let conn = Connection::open(&database).unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 6);
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM cg_fts_nodes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);
    drop(reopened);
    CodegraphStore::open(&database, PROJECT).unwrap();
    let rows_again: i64 = conn
        .query_row("SELECT count(*) FROM cg_fts_nodes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows_again, 1);
}
