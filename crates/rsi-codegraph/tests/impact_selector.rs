#![allow(clippy::unwrap_used, clippy::expect_used)]

use rsi_codegraph::{
    CodegraphStore, EvidenceFact, ExtractionContract, ExtractionMode, ExtractorIdentity, FactKey,
    FactProvenance, NodeFact, NodeIdentity, NodeKind, RelationFact, RelationKind,
    SourceFactBundle, SourceFile, SourceSpan, WorkspaceInstanceKey,
    impact::{
        ChangedFile, GraphImpactSource, GraphStatus, SelectionKind, WorkspaceImpactDependency,
        WorkspaceImpactMetadata, WorkspaceImpactPackage, select_tests,
    },
};
use uuid::Uuid;

const PROJECT: Uuid = Uuid::from_u128(0xcdcdcdcd_cdcd_4dcd_9dcd_cdcdcdcdcdcd);
const CHANGED_PATH: &str = "crates/demo/src/widget.rs";
const CHANGED_SOURCE: &str = "widget";
const DEPENDENT_PATH: &str = "crates/app/src/widget_user.rs";
const DEPENDENT_SOURCE: &str = "widget_user";

fn span(path: &str, text: &str) -> SourceSpan {
    SourceSpan {
        path: path.into(),
        start_byte: 0,
        end_byte: text.len(),
        start_line: 1,
        start_column: 1,
        end_line: 1,
        end_column: text.len() + 1,
    }
}

fn node(path: &str, name: &str) -> NodeFact {
    let span = span(path, name);
    NodeFact {
        key: FactKey(name.into()),
        identity: NodeIdentity {
            language: "rust".into(),
            qualified_name: name.into(),
            disambiguator: "v1:declaration".into(),
        },
        kind: NodeKind::Function,
        name: name.into(),
        span: span.clone(),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "declaration".into(),
            span,
        }],
    }
}

fn bundle() -> SourceFactBundle {
    SourceFactBundle {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "impact-selector-fixture".into(),
                version: "1".into(),
            },
        },
        files: vec![
            SourceFile {
                relative_path: CHANGED_PATH.into(),
                bytes: CHANGED_SOURCE.as_bytes().to_vec(),
            },
            SourceFile {
                relative_path: DEPENDENT_PATH.into(),
                bytes: DEPENDENT_SOURCE.as_bytes().to_vec(),
            },
        ],
        nodes: vec![node(CHANGED_PATH, "widget"), node(DEPENDENT_PATH, "widget_user")],
        relations: vec![RelationFact {
            key: FactKey("widget-user-calls-widget".into()),
            owner_file: DEPENDENT_PATH.into(),
            site_anchor: "v1:widget-user-calls-widget".into(),
            kind: RelationKind::Calls,
            source: FactKey("widget_user".into()),
            target: FactKey("widget".into()),
            provenance: FactProvenance::Extracted,
            evidence: vec![EvidenceFact {
                label: "call site".into(),
                span: span(DEPENDENT_PATH, DEPENDENT_SOURCE),
            }],
        }],
        unresolved_references: Vec::new(),
    }
}

fn metadata() -> WorkspaceImpactMetadata {
    WorkspaceImpactMetadata {
        workspace_members: vec!["demo-id".into(), "app-id".into()],
        packages: vec![
            WorkspaceImpactPackage {
                id: "demo-id".into(),
                name: "demo".into(),
                dependencies: Vec::new(),
            },
            WorkspaceImpactPackage {
                id: "app-id".into(),
                name: "app".into(),
                dependencies: vec![WorkspaceImpactDependency {
                    name: "demo".into(),
                    path: Some("../demo".into()),
                }],
            },
        ],
    }
}

#[test]
fn fresh_graph_widens_selection_to_reverse_dependency_module() {
    let directory = tempfile::tempdir().unwrap();
    let mut store =
        CodegraphStore::open(directory.path().join("codegraph.sqlite"), PROJECT).unwrap();
    let workspace = store.scope(WorkspaceInstanceKey::Primary).workspace_id();
    store
        .publish(&store.scope(WorkspaceInstanceKey::Primary), &bundle())
        .unwrap();
    let selection = select_tests(
        &[ChangedFile::new(CHANGED_PATH, CHANGED_SOURCE)],
        &metadata(),
        Some(GraphImpactSource {
            store: &store,
            workspace_id: workspace,
        }),
    )
    .unwrap();

    assert_eq!(selection.graph_status, GraphStatus::Ready);
    assert!(selection
        .selections
        .iter()
        .any(|selection| selection.package == "demo"
            && selection.filter.as_deref() == Some("widget")
            && selection.kind == SelectionKind::ChangedModule));
    assert!(selection
        .selections
        .iter()
        .any(|selection| selection.package == "app"
            && selection.filter.as_deref() == Some("widget_user")
            && selection.kind == SelectionKind::GraphImpact));
}

#[test]
fn stale_graph_falls_back_to_module_mapping() {
    let directory = tempfile::tempdir().unwrap();
    let mut store =
        CodegraphStore::open(directory.path().join("codegraph.sqlite"), PROJECT).unwrap();
    let workspace = store.scope(WorkspaceInstanceKey::Primary).workspace_id();
    store
        .publish(&store.scope(WorkspaceInstanceKey::Primary), &bundle())
        .unwrap();
    let selection = select_tests(
        &[ChangedFile::new(CHANGED_PATH, "different widget")],
        &metadata(),
        Some(GraphImpactSource {
            store: &store,
            workspace_id: workspace,
        }),
    )
    .unwrap();

    assert_eq!(selection.graph_status, GraphStatus::Stale);
    assert!(selection
        .selections
        .iter()
        .any(|selection| selection.package == "app" && selection.filter.is_none()));
    assert!(!selection
        .selections
        .iter()
        .any(|selection| selection.kind == SelectionKind::GraphImpact));
}
