#![allow(clippy::expect_used, clippy::unwrap_used)] // Focused acceptance fixture.

use rsi_codegraph::extract::extract_file;
use rsi_codegraph::staged::PARSER_MAX_FILE_BYTES;
use rsi_codegraph::{
    CodegraphError, CodegraphStore, EvidenceFact, ExtractionContract, ExtractionMode,
    ExtractorIdentity, FactKey, FactProvenance, FileDiagnostic, NodeFact, NodeIdentity, NodeKind,
    PerFileFacts, PublishFault, RelationFact, RelationKind, SourceFactBundle, SourceFile,
    SourceSpan, SourceVersion, StagedExtraction, UnresolvedReferenceFact, UnresolvedReferenceKind,
    WorkspaceInstanceKey,
};
use rusqlite::{Connection, params};
use uuid::Uuid;

fn manifest() -> StagedExtraction {
    StagedExtraction {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "s1-synthetic".into(),
                version: "1".into(),
            },
        },
        grammar_version: "grammar-1".into(),
        rule_version: "rules-1".into(),
        normalization_version: "normalization-1".into(),
        config_digest: "config-1".into(),
    }
}

fn span(path: &str) -> SourceSpan {
    SourceSpan {
        path: path.into(),
        start_byte: 0,
        end_byte: 1,
        start_line: 1,
        start_column: 1,
        end_line: 1,
        end_column: 2,
    }
}

fn file(path: &str, name: &str, bytes: usize) -> PerFileFacts {
    let mut source = vec![b'x'; bytes];
    source[0] = b'n';
    let site = span(path);
    PerFileFacts {
        file: SourceFile {
            relative_path: path.into(),
            bytes: source,
        },
        nodes: vec![NodeFact {
            key: FactKey(format!("node:{path}")),
            identity: NodeIdentity {
                language: "rust".into(),
                qualified_name: format!("symbol:{path}"),
                disambiguator: "v1:synthetic".into(),
            },
            kind: NodeKind::File,
            name: name.into(),
            span: site.clone(),
            provenance: FactProvenance::Extracted,
            evidence: vec![EvidenceFact {
                label: "source marker".into(),
                span: site,
            }],
        }],
        relations: Vec::new(),
        unresolved_references: Vec::new(),
        diagnostic: FileDiagnostic::Parsed,
    }
}

fn version(facts: &PerFileFacts) -> SourceVersion {
    SourceVersion {
        relative_path: facts.file.relative_path.clone(),
        source_digest: blake3::hash(&facts.file.bytes).to_hex().to_string(),
    }
}

fn synthetic_inventory(entries: &[(&str, usize)]) -> Vec<SourceVersion> {
    entries
        .iter()
        .map(|(path, bytes)| version(&file(path, "inventory", *bytes)))
        .collect()
}

fn link(owner: &str, key: &str, source: &str, target: &str) -> RelationFact {
    RelationFact {
        key: FactKey(key.into()),
        owner_file: owner.into(),
        site_anchor: format!("v1:{owner}:{source}:{target}"),
        kind: RelationKind::Calls,
        source: FactKey(source.into()),
        target: FactKey(target.into()),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "site".into(),
            span: span(owner),
        }],
    }
}

fn store() -> (tempfile::TempDir, CodegraphStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = CodegraphStore::open(directory.path().join("graph.sqlite"), Uuid::nil()).unwrap();
    (directory, store)
}

#[test]
fn stages_more_than_s0_file_and_byte_caps_with_large_file_and_order_independent_digest() {
    let (_left_dir, mut left) = store();
    let (_right_dir, mut right) = store();
    let left_scope = left.scope(WorkspaceInstanceKey::Primary);
    let right_scope = right.scope(WorkspaceInstanceKey::Primary);
    let mut files = (0..129)
        .map(|index| {
            file(
                &format!("src/file-{index:03}.rs"),
                &format!("file-{index:03}"),
                131_072,
            )
        })
        .collect::<Vec<_>>();
    files.push(file("src/large.rs", "large", 1_805_090));
    let inventory = files.iter().map(version).collect::<Vec<_>>();
    let left_run = left
        .begin_staged(&left_scope, &manifest(), &inventory)
        .unwrap();
    let right_run = right
        .begin_staged(&right_scope, &manifest(), &inventory)
        .unwrap();
    assert!(
        files
            .iter()
            .map(|item| item.file.bytes.len())
            .sum::<usize>()
            > 16_777_216
    );
    for facts in &files {
        left.stage_file(&left_run, facts).unwrap();
    }
    for facts in files.iter().rev() {
        right.stage_file(&right_run, facts).unwrap();
    }
    assert!(matches!(
        left.current_ready(left_scope.workspace_id()),
        Err(CodegraphError::NoReadySnapshot)
    ));
    let first = left.publish_staged(&left_run, PublishFault::None).unwrap();
    let second = right
        .publish_staged(&right_run, PublishFault::None)
        .unwrap();
    assert_eq!(first.snapshot_digest, second.snapshot_digest);
    assert_eq!(first.graph_digest, second.graph_digest);
    assert_eq!(
        left.current_completeness(left_scope.workspace_id())
            .unwrap()
            .total_files,
        130
    );
    let large = left
        .search_name(left_scope.workspace_id(), "large", 1)
        .unwrap();
    assert_eq!(large.nodes.len(), 1);
    assert_eq!(
        large.nodes[0].evidence[0].source_digest,
        blake3::hash(&files.last().unwrap().file.bytes)
            .to_hex()
            .to_string()
    );
}

#[test]
fn changed_and_deleted_owners_replace_facts_and_failure_preserves_last_ready() {
    let (_dir, mut store) = store();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let run = store
        .begin_staged(
            &scope,
            &manifest(),
            &synthetic_inventory(&[("src/a.rs", 8), ("src/b.rs", 8)]),
        )
        .unwrap();
    let mut a = file("src/a.rs", "first", 8);
    let b = file("src/b.rs", "second", 8);
    a.relations.push(RelationFact {
        key: FactKey("a-to-b".into()),
        owner_file: "src/a.rs".into(),
        site_anchor: "v1:link".into(),
        kind: RelationKind::Calls,
        source: FactKey("node:src/a.rs".into()),
        target: FactKey("node:src/b.rs".into()),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "site".into(),
            span: span("src/a.rs"),
        }],
    });
    store.stage_file(&run, &a).unwrap();
    store.stage_file(&run, &b).unwrap();
    let ready = store.publish_staged(&run, PublishFault::None).unwrap();
    let first = store.search_name(scope.workspace_id(), "first", 1).unwrap();
    assert_eq!(
        store
            .explain(scope.workspace_id(), first.nodes[0].id)
            .unwrap()
            .directed_relations
            .len(),
        1
    );

    let mut changed = file("src/a.rs", "changed", 9);
    changed.file.bytes[0] = b'c';
    let next = store
        .begin_staged(&scope, &manifest(), &[version(&changed)])
        .unwrap();
    store.stage_file(&next, &changed).unwrap();
    assert_eq!(store.current_ready(scope.workspace_id()).unwrap(), ready);
    assert_eq!(
        store
            .search_name(scope.workspace_id(), "first", 1)
            .unwrap()
            .nodes
            .len(),
        1
    );
    assert!(matches!(
        store.publish_staged(&next, PublishFault::BeforeHeadFlip),
        Err(CodegraphError::InjectedPublishFailure)
    ));
    assert_eq!(store.current_ready(scope.workspace_id()).unwrap(), ready);
    let published = store.publish_staged(&next, PublishFault::None).unwrap();
    assert_eq!(published.generation, ready.generation + 1);
    assert_eq!(
        store
            .current_completeness(scope.workspace_id())
            .unwrap()
            .total_files,
        1
    );
    assert_eq!(
        store
            .search_path(scope.workspace_id(), "src/a.rs", 2)
            .unwrap()
            .nodes[0]
            .name,
        "changed"
    );
    assert!(store.file_id(scope.workspace_id(), "src/b.rs").is_err());
    assert_eq!(
        store
            .explain(
                scope.workspace_id(),
                store
                    .search_name(scope.workspace_id(), "changed", 1)
                    .unwrap()
                    .nodes[0]
                    .id
            )
            .unwrap()
            .directed_relations
            .len(),
        0
    );
}

#[test]
fn typed_diagnostics_degrade_completeness_and_reject_resolved_edges() {
    let (_dir, mut store) = store();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let broken = extract_file(SourceFile {
        relative_path: "src/broken.rs".into(),
        bytes: b"fn broken( {\n".to_vec(),
    });
    assert!(matches!(&broken.diagnostic, FileDiagnostic::ParseErrors { count } if *count > 0));
    assert!(broken.nodes.is_empty());
    let parsed = file("src/good.rs", "good", 10);
    let run = store
        .begin_staged(&scope, &manifest(), &[version(&broken), version(&parsed)])
        .unwrap();
    let mut forged = broken.clone();
    forged.relations.push(RelationFact {
        key: FactKey("false-edge".into()),
        owner_file: "src/broken.rs".into(),
        site_anchor: "v1:bad".into(),
        kind: RelationKind::Calls,
        source: FactKey("node:src/broken.rs".into()),
        target: FactKey("node:src/good.rs".into()),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "bad".into(),
            span: span("src/broken.rs"),
        }],
    });
    assert!(matches!(
        store.stage_file(&run, &forged),
        Err(CodegraphError::InvalidInput(_))
    ));
    store.stage_file(&run, &broken).unwrap();
    store.stage_file(&run, &parsed).unwrap();
    store.publish_staged(&run, PublishFault::None).unwrap();
    let completeness = store.current_completeness(scope.workspace_id()).unwrap();
    assert_eq!(
        (
            completeness.parsed_files,
            completeness.degraded_files,
            completeness.total_files,
        ),
        (1, 1, 2)
    );
    assert_eq!(
        store
            .current_file_diagnostic(scope.workspace_id(), "src/broken.rs")
            .unwrap(),
        broken.diagnostic
    );
    let good_node = store.search_name(scope.workspace_id(), "good", 1).unwrap();
    assert_eq!(
        store
            .explain(scope.workspace_id(), good_node.nodes[0].id)
            .unwrap()
            .directed_relations
            .len(),
        0
    );
}

#[test]
fn degraded_corpus_completeness_reports_each_count_in_its_column() {
    let (_dir, mut store) = store();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let files = [
        ("src/good.rs", b"pub fn good() {}\n".as_slice()),
        ("src/broken.rs", b"fn broken( {\n".as_slice()),
        ("src/worse.rs", b"fn worse( {\n".as_slice()),
    ]
    .into_iter()
    .map(|(relative_path, bytes)| {
        extract_file(SourceFile {
            relative_path: relative_path.into(),
            bytes: bytes.to_vec(),
        })
    })
    .collect::<Vec<_>>();
    assert_eq!(files[0].diagnostic, FileDiagnostic::Parsed);
    assert!(
        files[1..]
            .iter()
            .all(|file| matches!(file.diagnostic, FileDiagnostic::ParseErrors { .. }))
    );
    let versions = files.iter().map(version).collect::<Vec<_>>();
    let run = store.begin_staged(&scope, &manifest(), &versions).unwrap();
    for file in &files {
        store.stage_file(&run, file).unwrap();
    }
    store.publish_staged(&run, PublishFault::None).unwrap();
    let completeness = store.current_completeness(scope.workspace_id()).unwrap();
    assert_eq!(
        (
            completeness.parsed_files,
            completeness.degraded_files,
            completeness.total_files,
        ),
        (1, 2, 3)
    );
}

#[test]
fn zero_fact_parse_errors_require_exact_source_and_count() {
    let (_dir, mut store) = store();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let broken = extract_file(SourceFile {
        relative_path: "src/broken.rs".into(),
        bytes: b"fn broken( {\n".to_vec(),
    });
    assert!(matches!(&broken.diagnostic, FileDiagnostic::ParseErrors { count } if *count > 0));
    assert!(broken.nodes.is_empty());
    let clean = SourceFile {
        relative_path: "src/clean.rs".into(),
        bytes: b"\n".to_vec(),
    };
    assert_eq!(
        extract_file(clean.clone()).diagnostic,
        FileDiagnostic::Parsed
    );
    let forged_clean = PerFileFacts {
        file: clean,
        nodes: Vec::new(),
        relations: Vec::new(),
        unresolved_references: Vec::new(),
        diagnostic: broken.diagnostic.clone(),
    };
    let run = store
        .begin_staged(
            &scope,
            &manifest(),
            &[version(&broken), version(&forged_clean)],
        )
        .unwrap();
    let mut forged_count = broken.clone();
    if let FileDiagnostic::ParseErrors { count } = &mut forged_count.diagnostic {
        *count += 1;
    }
    assert!(matches!(
        store.stage_file(&run, &forged_count),
        Err(CodegraphError::InvalidInput(_))
    ));
    assert!(matches!(
        store.stage_file(&run, &forged_clean),
        Err(CodegraphError::InvalidInput(_))
    ));
    store.stage_file(&run, &broken).unwrap();
}

#[test]
fn bounds_and_missing_cross_file_endpoint_fail_visibly_without_head_change() {
    let (_dir, mut store) = store();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let run = store
        .begin_staged(
            &scope,
            &manifest(),
            &synthetic_inventory(&[("src/one.rs", 8)]),
        )
        .unwrap();
    let large = file("src/too-large.rs", "large", 8 * 1024 * 1024 + 1);
    assert!(matches!(
        store.stage_file(&run, &large),
        Err(CodegraphError::LimitExceeded { .. })
    ));
    let mut one = file("src/one.rs", "one", 8);
    one.relations.push(RelationFact {
        key: FactKey("missing-target".into()),
        owner_file: "src/one.rs".into(),
        site_anchor: "v1:missing".into(),
        kind: RelationKind::Calls,
        source: FactKey("node:src/one.rs".into()),
        target: FactKey("node:src/missing.rs".into()),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "site".into(),
            span: span("src/one.rs"),
        }],
    });
    store.stage_file(&run, &one).unwrap();
    assert!(matches!(
        store.publish_staged(&run, PublishFault::None),
        Err(CodegraphError::MissingNode(_))
    ));
    assert!(matches!(
        store.current_ready(scope.workspace_id()),
        Err(CodegraphError::NoReadySnapshot)
    ));
}

#[test]
fn declared_inventory_must_be_complete_before_ready_publication() {
    let (_dir, mut store) = store();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let run = store
        .begin_staged(
            &scope,
            &manifest(),
            &synthetic_inventory(&[("src/a.rs", 8), ("src/b.rs", 8)]),
        )
        .unwrap();
    assert!(matches!(
        store.stage_file(&run, &file("src/undeclared.rs", "other", 8)),
        Err(CodegraphError::InvalidInput(_))
    ));
    let mut changed_after_discovery = file("src/a.rs", "a", 8);
    changed_after_discovery.file.bytes[0] = b'c';
    assert!(matches!(
        store.stage_file(&run, &changed_after_discovery),
        Err(CodegraphError::InvalidInput(_))
    ));
    store.stage_file(&run, &file("src/a.rs", "a", 8)).unwrap();
    assert!(matches!(
        store.publish_staged(&run, PublishFault::None),
        Err(CodegraphError::InvalidInput(_))
    ));
    assert!(matches!(
        store.current_ready(scope.workspace_id()),
        Err(CodegraphError::NoReadySnapshot)
    ));
    store.stage_file(&run, &file("src/b.rs", "b", 8)).unwrap();
    assert_eq!(
        store
            .publish_staged(&run, PublishFault::None)
            .unwrap()
            .generation,
        1
    );
}

#[test]
fn per_file_retry_replaces_its_staged_facts_without_double_counting() {
    let (_dir, mut store) = store();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let first = file("src/retry.rs", "first", 8);
    let run = store
        .begin_staged(&scope, &manifest(), &[version(&first)])
        .unwrap();
    store.stage_file(&run, &first).unwrap();
    let corrected = file("src/retry.rs", "corrected", 8);
    store.stage_file(&run, &corrected).unwrap();
    store.publish_staged(&run, PublishFault::None).unwrap();
    assert_eq!(
        store
            .current_completeness(scope.workspace_id())
            .unwrap()
            .total_files,
        1
    );
    let result = store
        .search_path(scope.workspace_id(), "src/retry.rs", 2)
        .unwrap();
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(result.nodes[0].name, "corrected");
}

#[test]
fn wal_reader_sees_old_head_and_competing_writer_returns_retryable_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = CodegraphStore::open(&database, Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let first = file("src/a.rs", "first", 8);
    let run = store
        .begin_staged(&scope, &manifest(), &[version(&first)])
        .unwrap();
    store.stage_file(&run, &first).unwrap();
    let ready = store.publish_staged(&run, PublishFault::None).unwrap();

    let reader = Connection::open(&database).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let read_head = |connection: &Connection| -> i64 {
        connection
            .query_row(
                "SELECT generation FROM cg_workspace_heads WHERE workspace_id=?1",
                [scope.workspace_id().to_string()],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(read_head(&reader), ready.generation);
    let changed = file("src/a.rs", "changed", 9);
    let next = store
        .begin_staged(&scope, &manifest(), &[version(&changed)])
        .unwrap();
    store.stage_file(&next, &changed).unwrap();
    let published = store.publish_staged(&next, PublishFault::None).unwrap();
    assert_eq!(read_head(&reader), ready.generation);
    assert_eq!(
        store
            .current_ready(scope.workspace_id())
            .unwrap()
            .generation,
        published.generation
    );
    reader.execute_batch("COMMIT").unwrap();
    assert_eq!(read_head(&reader), published.generation);

    let third = file("src/a.rs", "third", 10);
    let blocked = store
        .begin_staged(&scope, &manifest(), &[version(&third)])
        .unwrap();
    store.stage_file(&blocked, &third).unwrap();
    let writer = Connection::open(&database).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(matches!(
        store.publish_staged(&blocked, PublishFault::None),
        Err(CodegraphError::RetryableWriterConflict)
    ));
    assert_eq!(
        store
            .current_ready(scope.workspace_id())
            .unwrap()
            .generation,
        published.generation
    );
    writer.execute_batch("ROLLBACK").unwrap();
    let retried = store.publish_staged(&blocked, PublishFault::None).unwrap();
    assert_eq!(retried.generation, published.generation + 1);
}

#[test]
fn duplicate_cross_owner_relation_key_rolls_back_to_previous_ready_head() {
    let (_dir, mut store) = store();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let a = file("src/a.rs", "a", 8);
    let b = file("src/b.rs", "b", 8);
    let owners = [version(&a), version(&b)];
    let first = store.begin_staged(&scope, &manifest(), &owners).unwrap();
    store.stage_file(&first, &a).unwrap();
    store.stage_file(&first, &b).unwrap();
    let ready = store.publish_staged(&first, PublishFault::None).unwrap();

    let mut a = a;
    let mut b = b;
    a.relations.push(link(
        "src/a.rs",
        "duplicate-key",
        "node:src/a.rs",
        "node:src/b.rs",
    ));
    b.relations.push(link(
        "src/b.rs",
        "duplicate-key",
        "node:src/b.rs",
        "node:src/a.rs",
    ));
    let second = store.begin_staged(&scope, &manifest(), &owners).unwrap();
    store.stage_file(&second, &a).unwrap();
    store.stage_file(&second, &b).unwrap();
    let error = store
        .publish_staged(&second, PublishFault::None)
        .unwrap_err();
    assert!(error.to_string().contains("relation_key"));
    assert_eq!(store.current_ready(scope.workspace_id()).unwrap(), ready);
    let node = store.search_name(scope.workspace_id(), "a", 1).unwrap();
    assert_eq!(
        store
            .explain(scope.workspace_id(), node.nodes[0].id)
            .unwrap()
            .directed_relations
            .len(),
        0
    );
}

#[test]
fn corrupted_stage_counters_fail_before_ready_head_flip() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = CodegraphStore::open(&database, Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let source = file("src/a.rs", "a", 8);
    let run = store
        .begin_staged(&scope, &manifest(), &[version(&source)])
        .unwrap();
    store.stage_file(&run, &source).unwrap();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE cg_stage_runs SET source_bytes=source_bytes+1 WHERE workspace_id=?1",
            [scope.workspace_id().to_string()],
        )
        .unwrap();
    let error = store.publish_staged(&run, PublishFault::None).unwrap_err();
    assert!(error.to_string().contains("stage counters disagree"));
    assert!(matches!(
        store.current_ready(scope.workspace_id()),
        Err(CodegraphError::NoReadySnapshot)
    ));
    connection.execute("UPDATE cg_stage_runs SET source_bytes=source_bytes-1,file_count=file_count+1 WHERE workspace_id=?1",
        [scope.workspace_id().to_string()]).unwrap();
    assert!(store.publish_staged(&run, PublishFault::None).is_err());
    connection
        .execute(
            "UPDATE cg_stage_runs SET file_count=file_count-1 WHERE workspace_id=?1",
            [scope.workspace_id().to_string()],
        )
        .unwrap();
    assert_eq!(
        store
            .publish_staged(&run, PublishFault::None)
            .unwrap()
            .generation,
        1
    );
}

#[test]
fn diagnostic_variants_and_elapsed_time_limit_are_visible() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = CodegraphStore::open(&database, Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let mut unsupported = file("src/unsupported.rs", "unsupported", 8);
    unsupported.nodes.clear();
    unsupported.diagnostic = FileDiagnostic::Unsupported {
        reason: "grammar unavailable".into(),
    };
    let mut non_utf8 = file("src/non-utf8.rs", "non-utf8", 8);
    non_utf8.file.bytes[0] = 0xff;
    non_utf8.nodes.clear();
    non_utf8.diagnostic = FileDiagnostic::NonUtf8;
    let mut oversized = file(
        "src/parser-limit.rs",
        "parser-limit",
        PARSER_MAX_FILE_BYTES + 1,
    );
    oversized.nodes.clear();
    oversized.diagnostic = FileDiagnostic::Oversize {
        bytes: PARSER_MAX_FILE_BYTES + 1,
    };
    let run = store
        .begin_staged(
            &scope,
            &manifest(),
            &[
                version(&unsupported),
                version(&non_utf8),
                version(&oversized),
            ],
        )
        .unwrap();
    store.stage_file(&run, &unsupported).unwrap();
    store.stage_file(&run, &non_utf8).unwrap();
    let mut wrongly_parsed = oversized.clone();
    wrongly_parsed.diagnostic = FileDiagnostic::Parsed;
    assert!(matches!(
        store.stage_file(&run, &wrongly_parsed),
        Err(CodegraphError::InvalidInput(_))
    ));
    store.stage_file(&run, &oversized).unwrap();
    store.publish_staged(&run, PublishFault::None).unwrap();
    assert_eq!(
        store
            .current_completeness(scope.workspace_id())
            .unwrap()
            .degraded_files,
        3
    );
    assert_eq!(
        store
            .current_file_diagnostic(scope.workspace_id(), "src/non-utf8.rs")
            .unwrap(),
        FileDiagnostic::NonUtf8
    );

    let stale = store
        .begin_staged(
            &scope,
            &manifest(),
            &synthetic_inventory(&[("src/new.rs", 8)]),
        )
        .unwrap();
    let connection = Connection::open(&database).unwrap();
    connection.execute("UPDATE cg_stage_runs SET started_at='2000-01-01T00:00:00.000000000Z' WHERE workspace_id=?1",
        [scope.workspace_id().to_string()]).unwrap();
    assert!(matches!(
        store.stage_file(&stale, &file("src/new.rs", "new", 8)),
        Err(CodegraphError::LimitExceeded { .. })
    ));
    assert_eq!(
        store
            .current_completeness(scope.workspace_id())
            .unwrap()
            .degraded_files,
        3
    );
}

#[test]
fn aggregate_source_fact_and_file_caps_reject_new_owner() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("graph.sqlite");
    let mut store = CodegraphStore::open(&database, Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let run = store
        .begin_staged(
            &scope,
            &manifest(),
            &synthetic_inventory(&[("src/new.rs", 8)]),
        )
        .unwrap();
    let mut connection = Connection::open(&database).unwrap();
    let workspace = scope.workspace_id().to_string();
    connection
        .execute(
            "INSERT INTO cg_stage_files VALUES (?1,'synthetic.rs','hash',?2,0,'{}')",
            params![workspace, 512 * 1024 * 1024],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE cg_stage_runs SET file_count=1,source_bytes=536870912 WHERE workspace_id=?1",
            [workspace.as_str()],
        )
        .unwrap();
    let new_owner = file("src/new.rs", "new", 8);
    assert!(matches!(
        store.stage_file(&run, &new_owner),
        Err(CodegraphError::LimitExceeded { .. })
    ));
    connection
        .execute(
            "UPDATE cg_stage_files SET source_bytes=0,fact_count=1000000 WHERE path='synthetic.rs'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE cg_stage_runs SET source_bytes=0,fact_count=1000000 WHERE workspace_id=?1",
            [workspace.as_str()],
        )
        .unwrap();
    assert!(matches!(
        store.stage_file(&run, &new_owner),
        Err(CodegraphError::LimitExceeded { .. })
    ));
    connection
        .execute(
            "UPDATE cg_stage_files SET fact_count=0 WHERE path='synthetic.rs'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE cg_stage_runs SET fact_count=0 WHERE workspace_id=?1",
            [workspace.as_str()],
        )
        .unwrap();
    let transaction = connection.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare("INSERT INTO cg_stage_files VALUES (?1,?2,'hash',0,0,'{}')")
            .unwrap();
        for index in 0..9_999 {
            insert
                .execute(params![workspace, format!("fake/{index:04}.rs")])
                .unwrap();
        }
    }
    transaction
        .execute(
            "UPDATE cg_stage_runs SET file_count=10000 WHERE workspace_id=?1",
            [workspace.as_str()],
        )
        .unwrap();
    transaction.commit().unwrap();
    assert!(matches!(
        store.stage_file(&run, &new_owner),
        Err(CodegraphError::LimitExceeded { .. })
    ));
    assert!(matches!(
        store.current_ready(scope.workspace_id()),
        Err(CodegraphError::NoReadySnapshot)
    ));
}

#[test]
fn populated_v2_database_migrates_forward_and_keeps_ready_head() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite");
    let project = Uuid::nil();
    // Open uses the exact released v1 and v2 migrations. S0 publication fills
    // every v2 fact family; removing only later-version tables restores a genuine
    // populated v2 database without hand-writing a partial schema projection.
    let mut store = CodegraphStore::open(&path, project).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let mut source = file("old.rs", "old", 8);
    let mut target = source.nodes[0].clone();
    target.key = FactKey("target".into());
    target.identity.qualified_name = "target".into();
    target.name = "target".into();
    source.nodes.push(target);
    source.relations.push(RelationFact {
        key: FactKey("old-to-target".into()),
        owner_file: "old.rs".into(),
        site_anchor: "v1:old-to-target".into(),
        kind: RelationKind::Calls,
        source: FactKey("node:old.rs".into()),
        target: FactKey("target".into()),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "call".into(),
            span: span("old.rs"),
        }],
    });
    source.unresolved_references.push(UnresolvedReferenceFact {
        key: FactKey("missing-site".into()),
        owner: FactKey("node:old.rs".into()),
        kind: UnresolvedReferenceKind::Call,
        raw_target: "missing".into(),
        span: span("old.rs"),
        provenance: FactProvenance::Extracted,
    });
    let bundle = SourceFactBundle {
        extraction: manifest().extraction,
        files: vec![source.file],
        nodes: source.nodes,
        relations: source.relations,
        unresolved_references: source.unresolved_references,
    };
    let ready = store.publish(&scope, &bundle).unwrap();
    let old_id = store
        .search_name(scope.workspace_id(), "old", 1)
        .unwrap()
        .nodes[0]
        .id;
    drop(store);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "DROP TABLE cg_fts_nodes;
        DROP TABLE cg_retained_relations;
        DROP TABLE cg_retained_nodes;
        DROP TABLE cg_generation_detail;
        DROP TABLE cg_index_manifest;
        DROP TABLE cg_index_runs;
        DROP TABLE cg_index_state;
        DROP TABLE cg_file_diagnostics;
        DROP TABLE cg_snapshot_completeness;
        DROP TABLE cg_stage_files;
        DROP TABLE cg_stage_inventory;
        DROP TABLE cg_stage_runs;
        PRAGMA user_version=2;",
        )
        .unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    drop(connection);
    let upgraded = CodegraphStore::open(&path, project).unwrap();
    assert_eq!(upgraded.current_ready(scope.workspace_id()).unwrap(), ready);
    let explained = upgraded.explain(scope.workspace_id(), old_id).unwrap();
    assert_eq!(explained.node.name, "old");
    assert_eq!(explained.directed_relations.len(), 1);
    assert_eq!(explained.unresolved_references.len(), 1);
    assert_eq!(
        upgraded
            .current_completeness(scope.workspace_id())
            .unwrap()
            .parsed_files,
        1
    );
    assert_eq!(
        upgraded
            .current_file_diagnostic(scope.workspace_id(), "old.rs")
            .unwrap(),
        FileDiagnostic::Parsed
    );
    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        6
    );
}
