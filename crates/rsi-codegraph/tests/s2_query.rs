#![allow(clippy::unwrap_used, clippy::expect_used)]
use rsi_codegraph::{
    CodegraphStore, EvidenceFact, ExtractionContract, ExtractionMode, ExtractorIdentity, FactKey,
    FactProvenance, NodeFact, NodeIdentity, NodeKind, RelationFact, RelationKind, SourceFactBundle,
    SourceFile, SourceSpan, WorkspaceInstanceKey,
    query::{
        ChangeKind, Direction, ProvenanceMode, QueryFilter, QueryLimits, SearchMode,
        SnapshotSelector, TruncationReason,
    },
};
use uuid::Uuid;

const PROJECT: Uuid = Uuid::from_u128(0xabababab_abab_4bab_8bab_abababababab);
const PATH: &str = "src/architecture.rs";
const SOURCE: &str = "binding action handler impact call1 call2 changed";

fn span(needle: &str) -> SourceSpan {
    let start = SOURCE.find(needle).unwrap();
    SourceSpan {
        path: PATH.into(),
        start_byte: start,
        end_byte: start + needle.len(),
        start_line: 1,
        start_column: start + 1,
        end_line: 1,
        end_column: start + needle.len() + 1,
    }
}
fn node(key: &str, kind: NodeKind, provenance: FactProvenance) -> NodeFact {
    NodeFact {
        key: FactKey(key.into()),
        identity: NodeIdentity {
            language: "rust".into(),
            qualified_name: key.into(),
            disambiguator: "v1:declaration".into(),
        },
        kind,
        name: key.into(),
        span: span(key),
        provenance,
        evidence: vec![EvidenceFact {
            label: "declaration".into(),
            span: span(key),
        }],
    }
}
fn relation(
    key: &str,
    source: &str,
    target: &str,
    site: &str,
    provenance: FactProvenance,
) -> RelationFact {
    RelationFact {
        key: FactKey(key.into()),
        owner_file: PATH.into(),
        site_anchor: format!("v1:{key}"),
        kind: RelationKind::Calls,
        source: FactKey(source.into()),
        target: FactKey(target.into()),
        provenance,
        evidence: vec![EvidenceFact {
            label: "call site".into(),
            span: span(site),
        }],
    }
}
fn bundle(changed: bool) -> SourceFactBundle {
    SourceFactBundle {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "s2-fixture".into(),
                version: "1".into(),
            },
        },
        files: vec![SourceFile {
            relative_path: PATH.into(),
            bytes: SOURCE.as_bytes().to_vec(),
        }],
        nodes: vec![
            node("binding", NodeKind::Function, FactProvenance::Extracted),
            node("action", NodeKind::EnumVariant, FactProvenance::Extracted),
            node("handler", NodeKind::Function, FactProvenance::Extracted),
            node("impact", NodeKind::Function, FactProvenance::Extracted),
        ],
        relations: vec![
            relation(
                "binding-action-1",
                "binding",
                "action",
                if changed { "call2" } else { "call1" },
                FactProvenance::Extracted,
            ),
            relation(
                "binding-action-2",
                "binding",
                "action",
                "call2",
                FactProvenance::Extracted,
            ),
            relation(
                "action-handler",
                "action",
                "handler",
                "handler",
                FactProvenance::Extracted,
            ),
            relation(
                "binding-handler",
                "binding",
                "handler",
                "handler",
                FactProvenance::Extracted,
            ),
            relation(
                "impact-binding",
                "impact",
                "binding",
                "impact",
                FactProvenance::Extracted,
            ),
        ],
        unresolved_references: vec![],
    }
}
fn setup() -> (tempfile::TempDir, CodegraphStore, Uuid) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = CodegraphStore::open(dir.path().join("graph.sqlite"), PROJECT).unwrap();
    let workspace = store.scope(WorkspaceInstanceKey::Primary).workspace_id();
    store
        .publish(&store.scope(WorkspaceInstanceKey::Primary), &bundle(false))
        .unwrap();
    (dir, store, workspace)
}
fn id(store: &CodegraphStore, workspace: Uuid, name: &str) -> Uuid {
    store.search_name(workspace, name, 1).unwrap()[0].id
}

#[test]
fn golden_architecture_and_impact_are_directed_and_evidenced() {
    let (_dir, store, workspace) = setup();
    let query = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let binding = id(&store, workspace, "binding");
    let action = id(&store, workspace, "action");
    let handler = id(&store, workspace, "handler");
    let path = query
        .path(
            binding,
            handler,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits::default(),
        )
        .unwrap();
    assert!(path.value.found);
    assert_eq!(path.value.graph.relations.len(), 1); // direct shortest route
    assert_eq!(path.value.graph.relations[0].source, binding);
    assert_eq!(path.value.graph.relations[0].target, handler);
    let explain = query
        .explain(action, &QueryFilter::default(), QueryLimits::default())
        .unwrap();
    assert_eq!(explain.value.node.as_ref().unwrap().id, action);
    assert_eq!(explain.value.relations.len(), 3); // two parallel call sites survive
    assert!(
        explain
            .value
            .relations
            .iter()
            .all(|r| !r.evidence.is_empty())
    );
    let impact = query
        .impact(handler, &QueryFilter::default(), QueryLimits::default())
        .unwrap();
    assert!(impact.value.nodes.iter().any(|n| n.id == action));
    assert!(impact.value.nodes.iter().any(|n| n.id == binding));
    assert_eq!(impact.meta.snapshot.workspace_id, workspace);
    assert!(impact.meta.complete);
    for relation in impact.value.relations {
        for evidence in relation.evidence {
            assert!(
                !SOURCE.as_bytes()[evidence.span.start_byte..evidence.span.end_byte].is_empty()
            );
            assert_eq!(
                evidence.source_digest,
                blake3::hash(SOURCE.as_bytes()).to_hex().to_string()
            );
        }
    }
}

#[test]
fn filtered_search_and_relation_explain_use_pinned_ready_facts() {
    let (_dir, store, workspace) = setup();
    let query = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let strict = QueryFilter::default();
    let matches = query
        .search_filtered(
            SearchMode::NameContains,
            "i",
            &[NodeKind::Function],
            Some("src/"),
            &strict,
            QueryLimits::default(),
        )
        .unwrap();
    assert!(matches.meta.complete);
    assert!(matches.value.iter().any(|node| node.name == "binding"));
    assert!(
        matches
            .value
            .iter()
            .all(|node| node.kind == NodeKind::Function)
    );
    assert!(matches.value.iter().all(|node| node.span.path == PATH));
    assert!(
        query
            .search_filtered(
                SearchMode::NameContains,
                "a",
                &[],
                Some("other"),
                &strict,
                QueryLimits::default(),
            )
            .unwrap()
            .value
            .is_empty()
    );
    assert!(
        query
            .search_filtered(
                SearchMode::NameContains,
                "a",
                &[],
                Some("../src"),
                &strict,
                QueryLimits::default(),
            )
            .is_err()
    );

    let binding = id(&store, workspace, "binding");
    let node_explain = query
        .explain(binding, &strict, QueryLimits::default())
        .unwrap();
    let relation_id = node_explain.value.relations[0].id;
    let relation_explain = query
        .explain_relation(relation_id, &strict, QueryLimits::default())
        .unwrap();
    assert!(relation_explain.meta.complete);
    assert_eq!(relation_explain.value.relations.len(), 1);
    assert_eq!(relation_explain.value.relations[0].id, relation_id);
    assert_eq!(relation_explain.value.nodes.len(), 2);
    assert!(!relation_explain.value.relations[0].evidence.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture exercises the complete seed and provenance matrix.
fn seed_modes_provenance_and_bounds_are_explicit() {
    let (dir, store, workspace) = setup();
    // v3 validates extracted-only ingestion. Inject labelled candidates in this
    // temporary database to exercise the reserved exploratory read contract.
    let conn = rusqlite::Connection::open(dir.path().join("graph.sqlite")).unwrap();
    conn.execute(
        "UPDATE cg_nodes SET provenance='Inferred' WHERE name='impact'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE cg_relations SET provenance='Inferred' WHERE relation_key='impact-binding'",
        [],
    )
    .unwrap();
    let query = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let strict = QueryFilter::default();
    let exploratory = QueryFilter {
        provenance: ProvenanceMode::Exploratory,
        relation_kinds: vec![],
    };
    assert_eq!(
        query
            .search(
                SearchMode::ExactName,
                "binding",
                &strict,
                QueryLimits::default()
            )
            .unwrap()
            .value
            .len(),
        1
    );
    assert_eq!(
        query
            .search(
                SearchMode::NameContains,
                "bind",
                &strict,
                QueryLimits::default()
            )
            .unwrap()
            .value
            .len(),
        1
    );
    assert_eq!(
        query
            .search(SearchMode::ExactPath, PATH, &strict, QueryLimits::default())
            .unwrap()
            .value
            .len(),
        3
    );
    assert_eq!(
        query
            .search(
                SearchMode::Fts,
                "architecture binding",
                &strict,
                QueryLimits::default()
            )
            .unwrap()
            .value
            .len(),
        1
    );
    assert_eq!(
        query
            .search(
                SearchMode::ExactName,
                "impact",
                &strict,
                QueryLimits::default()
            )
            .unwrap()
            .value
            .len(),
        0
    );
    assert_eq!(
        query
            .search(
                SearchMode::ExactName,
                "impact",
                &exploratory,
                QueryLimits::default()
            )
            .unwrap()
            .value
            .len(),
        1
    );
    let impact = id(&store, workspace, "impact");
    assert!(
        query
            .neighbors(
                impact,
                Direction::Outgoing,
                &exploratory,
                QueryLimits::default()
            )
            .unwrap()
            .value
            .nodes
            .len()
            > 1
    );
    let limits = QueryLimits {
        max_results: 1,
        ..QueryLimits::default()
    };
    let result = query
        .search(SearchMode::ExactPath, PATH, &strict, limits)
        .unwrap();
    assert_eq!(result.value.len(), 1);
    assert_eq!(result.meta.truncation, vec![TruncationReason::Results]);
    assert!(
        QueryLimits {
            max_frontier: 0,
            ..QueryLimits::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        QueryLimits {
            max_nodes: 513,
            ..QueryLimits::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn historical_pin_and_evidence_sensitive_diff() {
    let (_dir, mut store, workspace) = setup();
    let before = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap()
        .snapshot()
        .clone();
    store
        .publish(&store.scope(WorkspaceInstanceKey::Primary), &bundle(true))
        .unwrap();
    let old = store
        .query(workspace, SnapshotSelector::Generation(before.generation))
        .unwrap();
    let current = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let digest = store
        .query(
            workspace,
            SnapshotSelector::Digest(before.snapshot_digest.clone()),
        )
        .unwrap();
    assert_eq!(old.snapshot(), digest.snapshot());
    assert_ne!(
        old.snapshot().snapshot_digest,
        current.snapshot().snapshot_digest
    );
    let diff = old
        .diff(&current, &QueryFilter::default(), QueryLimits::default())
        .unwrap();
    assert_eq!(diff.value.relations.len(), 1);
    assert_eq!(diff.value.relations[0].kind, ChangeKind::Changed);
    assert_ne!(
        diff.value.relations[0].before.as_ref().unwrap().evidence,
        diff.value.relations[0].after.as_ref().unwrap().evidence
    );
    assert_eq!(diff.meta.snapshot.generation, before.generation);
}

#[test]
fn live_baseline_and_current_sessions_survive_external_retention() {
    let (dir, mut store, workspace) = setup();
    let database = dir.path().join("graph.sqlite");
    let baseline = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let old_generation = baseline.snapshot().generation;
    store
        .publish(&store.scope(WorkspaceInstanceKey::Primary), &bundle(true))
        .unwrap();
    let current = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    assert!(current.snapshot().generation > old_generation);

    // Simulate future generation retention on a separate WAL writer. The
    // already-open reader must keep the old facts, FTS rows and evidence.
    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE")
        .unwrap();
    for table in [
        "cg_fts_nodes",
        "cg_evidence",
        "cg_relations",
        "cg_nodes",
        "cg_unresolved_references",
        "cg_files",
        "cg_snapshots",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE workspace_id=?1 AND generation=?2"),
            rusqlite::params![workspace.to_string(), old_generation],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();

    let old_search = baseline
        .search(
            SearchMode::Fts,
            "architecture binding",
            &QueryFilter::default(),
            QueryLimits::default(),
        )
        .unwrap();
    assert_eq!(old_search.value.len(), 1);
    assert_eq!(old_search.meta.snapshot.generation, old_generation);
    let diff = baseline
        .diff(&current, &QueryFilter::default(), QueryLimits::default())
        .unwrap();
    assert_eq!(diff.value.relations.len(), 1);
    assert_eq!(diff.value.relations[0].kind, ChangeKind::Changed);
}

#[test]
fn fts_sessions_keep_workspace_ownership() {
    let (_dir, mut store, primary) = setup();
    let alternate_scope = store.scope(WorkspaceInstanceKey::RsiSandbox(Uuid::new_v4()));
    let alternate = alternate_scope.workspace_id();
    let mut facts = bundle(false);
    facts.nodes[0].name = "alternate-only".into();
    store.publish(&alternate_scope, &facts).unwrap();
    let primary_result = store
        .query(primary, SnapshotSelector::CurrentReady)
        .unwrap()
        .search(
            SearchMode::Fts,
            "architecture",
            &QueryFilter::default(),
            QueryLimits::default(),
        )
        .unwrap();
    let alternate_result = store
        .query(alternate, SnapshotSelector::CurrentReady)
        .unwrap()
        .search(
            SearchMode::Fts,
            "alternate-only",
            &QueryFilter::default(),
            QueryLimits::default(),
        )
        .unwrap();
    assert_eq!(primary_result.meta.snapshot.workspace_id, primary);
    assert_eq!(alternate_result.meta.snapshot.workspace_id, alternate);
    assert_eq!(alternate_result.value[0].name, "alternate-only");
    assert_eq!(
        primary_result
            .value
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        vec!["binding", "action", "handler", "impact"]
    );
}

#[test]
fn broad_fts_statement_returns_typed_timeout() {
    let (dir, store, workspace) = setup();
    let conn = rusqlite::Connection::open(dir.path().join("graph.sqlite")).unwrap();
    conn.execute(
        "WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x<30000)
         INSERT INTO cg_nodes(workspace_id,generation,node_id,node_key,kind,name,path,start_byte,end_byte,start_line,start_column,end_line,end_column,provenance)
         SELECT ?1,1,printf('10000000-0000-4000-8000-%012x',x),'bulk-'||x,'Function','bulk token',?2,0,1,1,1,1,2,'Extracted' FROM seq",
        rusqlite::params![workspace.to_string(), PATH],
    ).unwrap();
    conn.execute(
        "INSERT INTO cg_fts_nodes(name,path,workspace_id,generation,node_id,owner_path)
         SELECT name,path,workspace_id,generation,node_id,path FROM cg_nodes WHERE workspace_id=?1 AND generation=1 AND name='bulk token'",
        [workspace.to_string()],
    ).unwrap();
    let query = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let result = query
        .search(
            SearchMode::Fts,
            "bulk",
            &QueryFilter::default(),
            QueryLimits {
                timeout_ms: 1,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(result.meta.truncation.contains(&TruncationReason::Timeout));
    assert!(!result.meta.complete);
}

#[test]
fn evidence_fanout_over_hard_cap_reports_typed_counts() {
    let (dir, store, workspace) = setup();
    let binding = id(&store, workspace, "binding");
    let conn = rusqlite::Connection::open(dir.path().join("graph.sqlite")).unwrap();
    let source_digest = blake3::hash(SOURCE.as_bytes()).to_hex().to_string();
    for ordinal in 1..=70 {
        conn.execute(
            "INSERT INTO cg_evidence(workspace_id,generation,fact_type,fact_id,ordinal,label,path,start_byte,end_byte,start_line,start_column,end_line,end_column,source_digest,evidence_digest)
             VALUES (?1,1,'node',?2,?3,'extra',?4,0,1,1,1,1,2,?5,?6)",
            rusqlite::params![workspace.to_string(), binding.to_string(), ordinal, PATH, source_digest, format!("{ordinal:064x}")],
        ).unwrap();
    }
    let result = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap()
        .search(
            SearchMode::ExactName,
            "binding",
            &QueryFilter::default(),
            QueryLimits {
                max_evidence_per_fact: 4,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert_eq!(result.value[0].evidence.len(), 4);
    assert_eq!(result.meta.returned_evidence, 4);
    assert_eq!(result.meta.returned_nodes, 1);
    assert_eq!(result.meta.truncation, vec![TruncationReason::Evidence]);
    assert!(result.meta.estimated_output_bytes <= result.meta.limits.max_output_bytes);
}

#[test]
fn directed_cycle_has_finite_bounded_traversal() {
    let (dir, store, workspace) = setup();
    let binding = id(&store, workspace, "binding");
    let handler = id(&store, workspace, "handler");
    let conn = rusqlite::Connection::open(dir.path().join("graph.sqlite")).unwrap();
    conn.execute(
        "INSERT INTO cg_relations(workspace_id,generation,relation_id,relation_key,kind,source_id,target_id,provenance)
         VALUES (?1,1,'20000000-0000-4000-8000-000000000001','cycle','Calls',?2,?3,'Extracted')",
        rusqlite::params![workspace.to_string(), handler.to_string(), binding.to_string()],
    ).unwrap();
    let result = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap()
        .neighbors(
            binding,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits {
                max_depth: 8,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert_eq!(result.meta.returned_nodes, result.value.nodes.len());
    assert_eq!(result.meta.returned_relations, result.value.relations.len());
    assert_eq!(result.value.nodes.len(), 3);
    assert!(
        result
            .value
            .relations
            .iter()
            .any(|relation| relation.source == handler && relation.target == binding)
    );
}

#[test]
fn depth_frontier_and_output_limits_return_typed_prefixes() {
    let (_dir, store, workspace) = setup();
    let query = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let binding = id(&store, workspace, "binding");
    let graph = query
        .neighbors(
            binding,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits {
                max_depth: 1,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(graph.meta.truncation.contains(&TruncationReason::Depth));
    let frontier = query
        .neighbors(
            binding,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits {
                max_frontier: 1,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(
        frontier
            .meta
            .truncation
            .contains(&TruncationReason::Frontier)
    );
    let bytes = query
        .neighbors(
            binding,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits {
                max_output_bytes: 200,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(
        bytes
            .meta
            .truncation
            .contains(&TruncationReason::OutputBytes)
    );
    let tokens = query
        .neighbors(
            binding,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits {
                max_output_tokens: 200,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(
        tokens
            .meta
            .truncation
            .contains(&TruncationReason::OutputTokens)
    );
}

#[test]
fn equal_shortest_paths_are_deterministic_and_path_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = CodegraphStore::open(dir.path().join("graph.sqlite"), PROJECT).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let mut facts = bundle(false);
    facts
        .relations
        .retain(|relation| relation.key.0 != "binding-handler");
    store.publish(&scope, &facts).unwrap();
    let from = id(&store, scope.workspace_id(), "binding");
    let to = id(&store, scope.workspace_id(), "handler");
    let query = store
        .query(scope.workspace_id(), SnapshotSelector::CurrentReady)
        .unwrap();
    let one = query
        .path(
            from,
            to,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits {
                max_paths: 1,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(one.value.found);
    assert_eq!(one.value.graph.relations.len(), 2);
    assert_eq!(one.meta.truncation, vec![TruncationReason::Paths]);
    let many = query
        .path(
            from,
            to,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits::default(),
        )
        .unwrap();
    assert_eq!(many.value.alternatives.len(), 1);
    assert!(many.meta.complete);
    assert_ne!(
        many.value
            .graph
            .relations
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        many.value.alternatives[0]
            .relations
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        many.value,
        query
            .path(
                from,
                to,
                Direction::Outgoing,
                &QueryFilter::default(),
                QueryLimits::default()
            )
            .unwrap()
            .value
    );
}

#[test]
fn very_wide_hub_respects_relation_and_time_limits() {
    let (dir, store, workspace) = setup();
    let source = id(&store, workspace, "binding");
    let target = id(&store, workspace, "handler");
    let conn = rusqlite::Connection::open(dir.path().join("graph.sqlite")).unwrap();
    conn.execute(
        "WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x<100001)
         INSERT INTO cg_relations(workspace_id,generation,relation_id,relation_key,kind,source_id,target_id,provenance)
         SELECT ?1,1,printf('00000000-0000-4000-8000-%012x',x),'stress-'||x,'Calls',?2,?3,'Extracted' FROM seq",
        rusqlite::params![workspace.to_string(), source.to_string(), target.to_string()],
    ).unwrap();
    let query = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let bounded = query
        .neighbors(
            source,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits {
                max_relations: 4,
                timeout_ms: 5_000,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(bounded.meta.returned_relations <= 4);
    assert!(
        bounded
            .meta
            .truncation
            .contains(&TruncationReason::Relations)
    );
    let deadline = query
        .neighbors(
            source,
            Direction::Outgoing,
            &QueryFilter::default(),
            QueryLimits {
                max_relations: 4,
                timeout_ms: 1,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(
        deadline
            .meta
            .truncation
            .contains(&TruncationReason::Timeout)
    );
    let incoming = query
        .neighbors(
            target,
            Direction::Incoming,
            &QueryFilter::default(),
            QueryLimits {
                max_relations: 4,
                timeout_ms: 1,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert!(
        incoming
            .meta
            .truncation
            .contains(&TruncationReason::Timeout)
    );
}

#[test]
fn diff_id_scan_returns_typed_timeout_instead_of_sqlite_error() {
    let (dir, store, workspace) = setup();
    let conn = rusqlite::Connection::open(dir.path().join("graph.sqlite")).unwrap();
    conn.execute(
        "WITH RECURSIVE seq(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM seq WHERE x<30000)
         INSERT INTO cg_nodes(workspace_id,generation,node_id,node_key,kind,name,path,start_byte,end_byte,start_line,start_column,end_line,end_column,provenance)
         SELECT ?1,1,printf('30000000-0000-4000-8000-%012x',x),'diff-'||x,'Function','diff node',?2,0,1,1,1,1,2,'Extracted' FROM seq",
        rusqlite::params![workspace.to_string(), PATH],
    ).unwrap();
    let first = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let second = store
        .query(workspace, SnapshotSelector::CurrentReady)
        .unwrap();
    let result = first
        .diff(
            &second,
            &QueryFilter::default(),
            QueryLimits {
                timeout_ms: 1,
                ..QueryLimits::default()
            },
        )
        .unwrap();
    assert_eq!(result.meta.truncation, vec![TruncationReason::Timeout]);
    assert!(!result.meta.complete);
    assert!(result.value.nodes.is_empty());
    assert!(result.value.relations.is_empty());
}
