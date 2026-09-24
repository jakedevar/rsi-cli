//! CG-S0 acceptance on a pinned RSI source file, with an opt-in one-hop microcomparison.
#![allow(clippy::expect_used)] // Fixture and benchmark failures must stop the test at the source.

use std::{hint::black_box, path::PathBuf, time::Instant};

use rsi_codegraph::{
    CodegraphStore, EvidenceFact, ExtractionContract, ExtractionMode, ExtractorIdentity, FactKey,
    FactProvenance, NodeFact, NodeIdentity, NodeKind, ReadySnapshot, RelationFact, RelationKind,
    SourceFactBundle, SourceFile, SourceSpan, WorkspaceInstanceKey,
};
use rusqlite::{Connection, params};
use uuid::Uuid;

const SOURCE_PATH: &str = "crates/rsi-common/src/types.rs";
// Full file at 7432b48bfe287b49f2a3db4d40259b6ca8337890. Keep the
// acceptance corpus stable as the live repository source changes.
const SOURCE: &[u8] = include_bytes!("fixtures/s0-types-7432b48.txt");
const SOURCE_BLAKE3: &str = "e6a1dd2b85ebc141d4833e17e3fcbceaa828e745bf04b79876e6c3ac783ef842";
const SNAPSHOT_DIGEST: &str = "7290b663b08ccdc0bdc1e89278c1ae4591e5abd8a0624b25a7a88851b500d3e8";
const GRAPH_DIGEST: &str = "5a7ca27e313c10dac90ba7356b4afe3cee24b036990bdd24f9d23a5868522c80";
const PROJECT: Uuid = Uuid::from_u128(0xaaaa_aaaa_aaaa_4aaa_8aaa_aaaa_aaaa_aaaa);
const ITERATIONS: usize = 10_000;
const ONE_HOP_LIMIT: usize = 32;

#[allow(clippy::naive_bytecount)] // One source fixture needs no extra counting dependency.
fn span(needle: &str, occurrence: usize) -> SourceSpan {
    let text = std::str::from_utf8(SOURCE).expect("pinned Rust source is UTF-8");
    let start = text
        .match_indices(needle)
        .nth(occurrence)
        .expect("pinned source site exists")
        .0;
    let end = start + needle.len();
    let position = |offset: usize| {
        let prefix = &SOURCE[..offset];
        let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
        let column = prefix
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(offset + 1, |last_newline| offset - last_newline);
        (line, column)
    };
    let (start_line, start_column) = position(start);
    let (end_line, end_column) = position(end);
    SourceSpan {
        path: SOURCE_PATH.into(),
        start_byte: start,
        end_byte: end,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn node(key: &str, kind: NodeKind, name: &str, source: SourceSpan) -> NodeFact {
    NodeFact {
        key: FactKey(key.into()),
        identity: NodeIdentity {
            language: "rust".into(),
            qualified_name: format!("rsi_common::types::{key}"),
            disambiguator: "v1:declaration".into(),
        },
        kind,
        name: name.into(),
        span: source.clone(),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "pinned RSI declaration".into(),
            span: source,
        }],
    }
}

fn relation(
    key: &str,
    kind: RelationKind,
    source: &str,
    target: &str,
    site: &str,
    evidence: SourceSpan,
) -> RelationFact {
    RelationFact {
        key: FactKey(key.into()),
        owner_file: SOURCE_PATH.into(),
        site_anchor: format!("v1:{site}"),
        kind,
        source: FactKey(source.into()),
        target: FactKey(target.into()),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "pinned RSI syntax".into(),
            span: evidence,
        }],
    }
}

fn corpus() -> SourceFactBundle {
    let header = span(
        "use crate::provider_capabilities::ResolvedContextBudget;",
        0,
    );
    let helper = span("pub fn legal_children(parent: Option<SessionKind>)", 0);
    let test = span("fn legal_children_hierarchy_v1_matrix_is_exact()", 0);
    SourceFactBundle {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "cg-s0-pinned-rsi-fixture".into(),
                version: "1.0.0".into(),
            },
        },
        files: vec![SourceFile {
            relative_path: SOURCE_PATH.into(),
            bytes: SOURCE.to_vec(),
        }],
        nodes: vec![
            node("types_file", NodeKind::File, "types.rs", header),
            node(
                "legal_children",
                NodeKind::Function,
                "legal_children",
                helper,
            ),
            node(
                "legal_children_hierarchy_v1_matrix_is_exact",
                NodeKind::Function,
                "legal_children_hierarchy_v1_matrix_is_exact",
                test.clone(),
            ),
        ],
        relations: vec![
            relation(
                "file-owns-test",
                RelationKind::Contains,
                "types_file",
                "legal_children_hierarchy_v1_matrix_is_exact",
                "types/tests/legal_children_hierarchy_v1_matrix_is_exact",
                test,
            ),
            relation(
                "test-calls-helper",
                RelationKind::Calls,
                "legal_children_hierarchy_v1_matrix_is_exact",
                "legal_children",
                "types/tests/legal_children_hierarchy_v1_matrix_is_exact/call-0",
                span("legal_children(None)", 0),
            ),
        ],
        unresolved_references: Vec::new(),
    }
}

struct TemporaryCorpus {
    _directory: tempfile::TempDir,
    source_path: PathBuf,
    database_path: PathBuf,
    ready: ReadySnapshot,
    test_id: Uuid,
}

fn setup() -> TemporaryCorpus {
    assert_eq!(blake3::hash(SOURCE).to_hex().to_string(), SOURCE_BLAKE3);
    let directory = tempfile::tempdir().expect("temporary RSI corpus");
    let source_path = directory.path().join(SOURCE_PATH);
    std::fs::create_dir_all(source_path.parent().expect("source parent")).expect("corpus path");
    std::fs::write(&source_path, SOURCE).expect("materialize pinned corpus");
    let database_path = directory.path().join("codegraph.sqlite");
    let mut store = CodegraphStore::open(&database_path, PROJECT).expect("temporary store");
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let ready = store
        .publish(&scope, &corpus())
        .expect("pinned facts publish");
    assert_eq!(ready.snapshot_digest, SNAPSHOT_DIGEST);
    assert_eq!(ready.graph_digest, GRAPH_DIGEST);
    let test_id = store
        .search_name(
            scope.workspace_id(),
            "legal_children_hierarchy_v1_matrix_is_exact",
            1,
        )
        .expect("exact name search")[0]
        .id;
    TemporaryCorpus {
        _directory: directory,
        source_path,
        database_path,
        ready,
        test_id,
    }
}

fn matches_source(bytes: &[u8], source_span: &SourceSpan, expected: &str) -> bool {
    bytes[source_span.start_byte..source_span.end_byte] == *expected.as_bytes()
}

#[test]
#[allow(clippy::too_many_lines)] // One temporary corpus assertion covers every stored fact and evidence row.
fn pinned_rsi_question_has_exact_search_ownership_call_and_source_spans() {
    let corpus = setup();
    let store = CodegraphStore::open(&corpus.database_path, PROJECT).expect("reopen corpus");
    let source = std::fs::read(&corpus.source_path).expect("temporary source bytes");
    assert_eq!(source, SOURCE);
    let found = store
        .search_name(
            corpus.ready.workspace_id,
            "legal_children_hierarchy_v1_matrix_is_exact",
            1,
        )
        .expect("exact search");
    assert!(found.complete);
    assert_eq!(found.nodes.len(), 1);
    assert_eq!(found[0].id, corpus.test_id);
    let path_result = store
        .search_path(corpus.ready.workspace_id, SOURCE_PATH, 3)
        .expect("exact path search");
    assert!(path_result.complete);
    assert_eq!(path_result.nodes.len(), 3);
    let file_node = path_result
        .nodes
        .iter()
        .find(|node| node.kind == NodeKind::File)
        .expect("file identity");
    assert!(matches_source(
        &source,
        &file_node.span,
        "use crate::provider_capabilities::ResolvedContextBudget;"
    ));
    assert!(matches_source(
        &source,
        &found[0].span,
        "fn legal_children_hierarchy_v1_matrix_is_exact()"
    ));
    let explained = store
        .explain(corpus.ready.workspace_id, found[0].id)
        .expect("source-backed explanation");
    assert_eq!(explained.snapshot, corpus.ready);
    assert_eq!(explained.ownership.len(), 1);
    assert_eq!(explained.ownership[0].kind, RelationKind::Contains);
    assert_eq!(explained.ownership[0].target, found[0].id);
    assert!(matches_source(
        &source,
        &explained.node.evidence[0].span,
        "fn legal_children_hierarchy_v1_matrix_is_exact()"
    ));
    assert!(matches_source(
        &source,
        &explained.ownership[0].evidence[0].span,
        "fn legal_children_hierarchy_v1_matrix_is_exact()"
    ));
    assert_eq!(explained.directed_relations.len(), 1);
    assert_eq!(explained.directed_relations[0].kind, RelationKind::Calls);
    assert_eq!(explained.directed_relations[0].source, found[0].id);
    let helper = store
        .search_name(corpus.ready.workspace_id, "legal_children", 1)
        .expect("helper search");
    assert!(matches_source(
        &source,
        &helper[0].span,
        "pub fn legal_children(parent: Option<SessionKind>)"
    ));
    assert_eq!(explained.directed_relations[0].target, helper[0].id);
    assert!(matches_source(
        &source,
        &explained.directed_relations[0].evidence[0].span,
        "legal_children(None)"
    ));
    // The fresh bundle has exactly three node and two relation evidence rows.
    // Check every returned persisted row against independent literal source text.
    let evidence_cases = [
        (
            file_node.evidence.as_slice(),
            "use crate::provider_capabilities::ResolvedContextBudget;",
        ),
        (
            helper[0].evidence.as_slice(),
            "pub fn legal_children(parent: Option<SessionKind>)",
        ),
        (
            explained.node.evidence.as_slice(),
            "fn legal_children_hierarchy_v1_matrix_is_exact()",
        ),
        (
            explained.ownership[0].evidence.as_slice(),
            "fn legal_children_hierarchy_v1_matrix_is_exact()",
        ),
        (
            explained.directed_relations[0].evidence.as_slice(),
            "legal_children(None)",
        ),
    ];
    for (rows, literal) in evidence_cases {
        assert_eq!(rows.len(), 1);
        let evidence = &rows[0];
        assert_eq!(evidence.source_digest, SOURCE_BLAKE3);
        assert!(matches_source(&source, &evidence.span, literal));
    }
    let connection = Connection::open(&corpus.database_path).expect("evidence counts");
    let persisted_counts: (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT
               (SELECT COUNT(*) FROM cg_nodes),
               (SELECT COUNT(*) FROM cg_relations),
               (SELECT COUNT(*) FROM cg_evidence WHERE fact_type='node'),
               (SELECT COUNT(*) FROM cg_evidence WHERE fact_type='relation')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("persisted fact counts");
    assert_eq!(persisted_counts, (3, 2, 3, 2));
    println!(
        "pinned_source_blake3={SOURCE_BLAKE3} snapshot_digest={} graph_digest={}",
        corpus.ready.snapshot_digest, corpus.ready.graph_digest
    );
}

type DirectedEdge = (String, String, String);

fn checked_one_hop(mut edges: Vec<DirectedEdge>) -> rusqlite::Result<Vec<DirectedEdge>> {
    edges.sort();
    edges.dedup_by(|left, right| left.0 == right.0);
    if edges.len() > ONE_HOP_LIMIT {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(edges)
}

fn one_hop_cte(
    connection: &Connection,
    ready: &ReadySnapshot,
    node: Uuid,
) -> rusqlite::Result<Vec<DirectedEdge>> {
    let mut statement = connection.prepare(
        "WITH RECURSIVE walk(depth,node_id,relation_id,source_id,target_id) AS (
             SELECT 0,?3,NULL,NULL,NULL
             UNION ALL
             SELECT walk.depth+1,
                    CASE WHEN r.source_id=walk.node_id THEN r.target_id ELSE r.source_id END,
                    r.relation_id,r.source_id,r.target_id
             FROM walk JOIN cg_relations r
               ON r.source_id=walk.node_id OR r.target_id=walk.node_id
             WHERE walk.depth<1 AND r.workspace_id=?1 AND r.generation=?2
         )
         SELECT relation_id,source_id,target_id FROM walk WHERE depth=1
         ORDER BY relation_id LIMIT ?4",
    )?;
    let edges = statement
        .query_map(
            params![
                ready.workspace_id.to_string(),
                ready.generation,
                node.to_string(),
                ONE_HOP_LIMIT + 1
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    checked_one_hop(edges)
}

fn one_hop_adjacency(
    connection: &Connection,
    ready: &ReadySnapshot,
    node: Uuid,
) -> rusqlite::Result<Vec<DirectedEdge>> {
    let mut edges = Vec::new();
    for endpoint in ["source_id", "target_id"] {
        let sql = format!(
            "SELECT relation_id,source_id,target_id FROM cg_relations
             WHERE workspace_id=?1 AND generation=?2 AND {endpoint}=?3
             ORDER BY relation_id LIMIT ?4"
        );
        let mut statement = connection.prepare(&sql)?;
        edges.extend(
            statement
                .query_map(
                    params![
                        ready.workspace_id.to_string(),
                        ready.generation,
                        node.to_string(),
                        ONE_HOP_LIMIT + 1
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    checked_one_hop(edges)
}

#[test]
fn one_hop_projections_reject_overflow_instead_of_truncating() {
    let corpus = setup();
    let connection = Connection::open(&corpus.database_path).expect("overflow fixture database");
    let helper_id: String = connection
        .query_row(
            "SELECT target_id FROM cg_relations WHERE relation_key='test-calls-helper'",
            [],
            |row| row.get(0),
        )
        .expect("helper endpoint");
    for ordinal in 0..=ONE_HOP_LIMIT {
        connection
            .execute(
                "INSERT INTO cg_relations(workspace_id,generation,relation_id,relation_key,kind,source_id,target_id,provenance)
                 VALUES (?1,?2,?3,?4,'Calls',?5,?6,'Extracted')",
                params![
                    corpus.ready.workspace_id.to_string(),
                    corpus.ready.generation,
                    Uuid::new_v4().to_string(),
                    format!("overflow-{ordinal}"),
                    corpus.test_id.to_string(),
                    helper_id,
                ],
            )
            .expect("bounded adversarial relation");
    }
    assert!(matches!(
        one_hop_cte(&connection, &corpus.ready, corpus.test_id),
        Err(rusqlite::Error::InvalidQuery)
    ));
    assert!(matches!(
        one_hop_adjacency(&connection, &corpus.ready, corpus.test_id),
        Err(rusqlite::Error::InvalidQuery)
    ));
}

/// Run explicitly with `cargo test -p rsi-codegraph --test s0_acceptance
/// one_hop_microcomparison -- --ignored --nocapture --test-threads=8`.
#[test]
#[ignore = "timing evidence is collected explicitly; it is not a pass/fail threshold"]
fn one_hop_microcomparison() {
    let corpus = setup();
    let connection = Connection::open(&corpus.database_path).expect("benchmark database");
    let cte = one_hop_cte(&connection, &corpus.ready, corpus.test_id).expect("CTE result");
    let adjacency =
        one_hop_adjacency(&connection, &corpus.ready, corpus.test_id).expect("adjacency result");
    assert_eq!(
        cte, adjacency,
        "IDs, direction, and multiplicity must agree"
    );
    assert_eq!(cte.len(), 2);
    assert!(cte.len() <= ONE_HOP_LIMIT);
    for _ in 0..100 {
        black_box(one_hop_cte(&connection, &corpus.ready, corpus.test_id).expect("CTE warmup"));
        black_box(
            one_hop_adjacency(&connection, &corpus.ready, corpus.test_id)
                .expect("adjacency warmup"),
        );
    }
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(one_hop_cte(&connection, &corpus.ready, corpus.test_id).expect("CTE query"));
    }
    let cte_elapsed = start.elapsed();
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(
            one_hop_adjacency(&connection, &corpus.ready, corpus.test_id).expect("adjacency query"),
        );
    }
    let adjacency_elapsed = start.elapsed();
    println!(
        "one_hop_microcomparison arch={} sqlite={} source_blake3={} graph_digest={} nodes=3 relations=2 result_edges={} warmup=100 iterations={} cte_total_ns={} adjacency_total_ns={} cte_ns_per_query={} adjacency_ns_per_query={}",
        std::env::consts::ARCH,
        rusqlite::version(),
        SOURCE_BLAKE3,
        corpus.ready.graph_digest,
        cte.len(),
        ITERATIONS,
        cte_elapsed.as_nanos(),
        adjacency_elapsed.as_nanos(),
        cte_elapsed.as_nanos() / ITERATIONS as u128,
        adjacency_elapsed.as_nanos() / ITERATIONS as u128,
    );
}
