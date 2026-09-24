#![allow(clippy::expect_used, clippy::unwrap_used)] // Opt-in measurement on a temporary database.
use std::{
    collections::{BTreeSet, VecDeque},
    hint::black_box,
    time::Instant,
};

use rsi_codegraph::{
    CodegraphStore, EvidenceFact, ExtractionContract, ExtractionMode, ExtractorIdentity, FactKey,
    FactProvenance, NodeFact, NodeIdentity, NodeKind, RelationFact, RelationKind, SourceFactBundle,
    SourceFile, SourceSpan, WorkspaceInstanceKey,
    query::{Direction, QueryFilter, QueryLimits, SnapshotSelector},
};
use rusqlite::{Connection, params};
use uuid::Uuid;

const PROJECT: Uuid = Uuid::from_u128(0xeeeeeeee_eeee_4eee_8eee_eeeeeeeeeeee);
const PATH: &str = "src/bench.rs";
const COUNT: usize = 128;
const RUNS: usize = 100;

fn span(offset: usize) -> SourceSpan {
    SourceSpan {
        path: PATH.into(),
        start_byte: offset,
        end_byte: offset + 1,
        start_line: 1,
        start_column: offset + 1,
        end_line: 1,
        end_column: offset + 2,
    }
}
fn corpus() -> SourceFactBundle {
    let source = vec![b'x'; COUNT];
    let nodes = (0..COUNT)
        .map(|i| NodeFact {
            key: FactKey(format!("n{i:03}")),
            identity: NodeIdentity {
                language: "rust".into(),
                qualified_name: format!("bench::n{i:03}"),
                disambiguator: "v1:decl".into(),
            },
            kind: NodeKind::Function,
            name: format!("n{i:03}"),
            span: span(i),
            provenance: FactProvenance::Extracted,
            evidence: vec![EvidenceFact {
                label: "declaration".into(),
                span: span(i),
            }],
        })
        .collect();
    let relations = (0..COUNT)
        .flat_map(|i| {
            [1, 2].map(move |step| RelationFact {
                key: FactKey(format!("r{i:03}-{step}")),
                owner_file: PATH.into(),
                site_anchor: format!("v1:r{i:03}-{step}"),
                kind: RelationKind::Calls,
                source: FactKey(format!("n{i:03}")),
                target: FactKey(format!("n{:03}", (i + step) % COUNT)),
                provenance: FactProvenance::Extracted,
                evidence: vec![EvidenceFact {
                    label: "call".into(),
                    span: span(i),
                }],
            })
        })
        .collect();
    SourceFactBundle {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "s2-bench".into(),
                version: "1".into(),
            },
        },
        files: vec![SourceFile {
            relative_path: PATH.into(),
            bytes: source,
        }],
        nodes,
        relations,
        unresolved_references: vec![],
    }
}
fn adjacency_count(connection: &Connection, workspace: Uuid, generation: i64, seed: Uuid) -> usize {
    let mut stmt = connection
        .prepare(
            "SELECT source_id,target_id FROM cg_relations WHERE workspace_id=?1 AND generation=?2",
        )
        .unwrap();
    let edges = stmt
        .query_map(params![workspace.to_string(), generation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut visited = BTreeSet::from([seed.to_string()]);
    let mut queue = VecDeque::from([(seed.to_string(), 0)]);
    while let Some((current, depth)) = queue.pop_front() {
        if depth == 2 {
            continue;
        }
        for (source, target) in &edges {
            if source == &current && visited.insert(target.clone()) {
                queue.push_back((target.clone(), depth + 1));
            }
        }
    }
    visited.len()
}
fn cte_count(connection: &Connection, workspace: Uuid, generation: i64, seed: Uuid) -> i64 {
    connection
        .query_row(
            "WITH RECURSIVE walk(node,depth) AS (
           SELECT ?3,0 UNION ALL
           SELECT r.target_id,w.depth+1 FROM walk w JOIN cg_relations r
             ON r.workspace_id=?1 AND r.generation=?2 AND r.source_id=w.node
           WHERE w.depth<2
         ) SELECT count(DISTINCT node) FROM walk",
            params![workspace.to_string(), generation, seed.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

/// Run explicitly with `--ignored --nocapture --test-threads=8`; no live corpus is touched.
#[test]
#[ignore = "opt-in traversal strategy measurement on a temporary corpus"]
fn bounded_sqlite_vs_recursive_cte_vs_full_adjacency() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut store = CodegraphStore::open(&path, PROJECT).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let ready = store.publish(&scope, &corpus()).unwrap();
    let seed = store.search_name(scope.workspace_id(), "n000", 1).unwrap()[0].id;
    let query = store
        .query(scope.workspace_id(), SnapshotSelector::CurrentReady)
        .unwrap();
    let limits = QueryLimits {
        max_depth: 2,
        max_nodes: 128,
        max_relations: 256,
        timeout_ms: 5_000,
        ..QueryLimits::default()
    };
    let connection = Connection::open(path).unwrap();
    let answer = query
        .neighbors(seed, Direction::Outgoing, &QueryFilter::default(), limits)
        .unwrap();
    assert_eq!(
        answer.value.nodes.len(),
        adjacency_count(&connection, scope.workspace_id(), ready.generation, seed)
    );
    assert_eq!(
        i64::try_from(answer.value.nodes.len()).unwrap(),
        cte_count(&connection, scope.workspace_id(), ready.generation, seed)
    );
    let start = Instant::now();
    for _ in 0..RUNS {
        black_box(
            query
                .neighbors(seed, Direction::Outgoing, &QueryFilter::default(), limits)
                .unwrap(),
        );
    }
    let bounded = start.elapsed().as_nanos() / RUNS as u128;
    let start = Instant::now();
    for _ in 0..RUNS {
        black_box(cte_count(
            &connection,
            scope.workspace_id(),
            ready.generation,
            seed,
        ));
    }
    let cte = start.elapsed().as_nanos() / RUNS as u128;
    let start = Instant::now();
    for _ in 0..RUNS {
        black_box(adjacency_count(
            &connection,
            scope.workspace_id(),
            ready.generation,
            seed,
        ));
    }
    let adjacency = start.elapsed().as_nanos() / RUNS as u128;
    println!(
        "S2 strategy: nodes={COUNT} relations={} depth=2 runs={RUNS} bounded_sqlite_ns={bounded} cte_ns={cte} full_adjacency_ns={adjacency} answer_nodes={}",
        COUNT * 2,
        answer.value.nodes.len()
    );
}
