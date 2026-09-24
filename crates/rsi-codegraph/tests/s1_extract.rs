#![allow(clippy::expect_used, clippy::unwrap_used)] // Exact-source fixtures.

use rsi_codegraph::{
    CodegraphStore, ExtractionContract, ExtractionMode, ExtractorIdentity, FileDiagnostic,
    NodeKind, PublishFault, RelationKind, SourceFile, SourceVersion, StagedExtraction,
    WorkspaceInstanceKey, extract::extract_file, invalidate::ExtractionCache,
    resolve::resolve_unique,
};
use uuid::Uuid;

fn source(path: &str, text: &str) -> SourceFile {
    SourceFile {
        relative_path: path.into(),
        bytes: text.as_bytes().to_vec(),
    }
}

fn manifest() -> StagedExtraction {
    StagedExtraction {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "s1-extract".into(),
                version: "1".into(),
            },
        },
        grammar_version: "tree-sitter-1".into(),
        rule_version: "s1-1".into(),
        normalization_version: "s1-1".into(),
        config_digest: "fixture".into(),
    }
}

#[test]
fn rust_declarations_and_parallel_calls_preserve_exact_sites() {
    let file = source(
        "src/lib.rs",
        "mod inner { pub struct Thing; fn callee() {} fn caller() { callee(); callee(); } }\n",
    );
    let facts = extract_file(file.clone());
    assert_eq!(facts.diagnostic, FileDiagnostic::Parsed);
    for node in &facts.nodes {
        assert_eq!(node.span.path, file.relative_path);
        assert_eq!(node.span.start_line, 1);
        assert_eq!(
            node.span.end_line,
            if node.kind == NodeKind::File { 2 } else { 1 }
        );
        assert!(node.span.end_byte <= file.bytes.len());
    }
    assert!(
        facts
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::Module && node.name == "inner")
    );
    assert!(
        facts
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::Struct && node.name == "Thing")
    );
    assert!(
        facts
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::Function && node.name == "caller")
    );
    let calls = facts
        .unresolved_references
        .iter()
        .filter(|reference| reference.raw_target == "callee")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].key, calls[1].key);
    assert_ne!(calls[0].span.start_byte, calls[1].span.start_byte);
    assert!(
        facts
            .relations
            .iter()
            .all(|relation| relation.kind == RelationKind::Declares)
    );

    let temp = tempfile::tempdir().unwrap();
    let mut store = CodegraphStore::open(temp.path().join("graph.sqlite"), Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let run = store
        .begin_staged(
            &scope,
            &manifest(),
            &[SourceVersion {
                relative_path: file.relative_path.clone(),
                source_digest: blake3::hash(&file.bytes).to_hex().to_string(),
            }],
        )
        .unwrap();
    store.stage_file(&run, &facts).unwrap();
    let ready = store.publish_staged(&run, PublishFault::None).unwrap();
    assert_eq!(ready.generation, 1);
    assert_eq!(
        store
            .current_completeness(scope.workspace_id())
            .unwrap()
            .parsed_files,
        1
    );
}

#[test]
fn parser_errors_preserve_clean_regions_and_non_utf8_has_no_facts() {
    let broken = extract_file(source("src/broken.rs", "fn good() {}\nfn broken( {\n"));
    assert!(matches!(broken.diagnostic, FileDiagnostic::ParseErrors { count } if count > 0));
    assert_eq!(broken.nodes.len(), 1);
    assert_eq!(broken.nodes[0].name, "good");
    assert_eq!(broken.nodes[0].span.start_line, 1);
    assert!(broken.relations.is_empty());
    let invalid = extract_file(SourceFile {
        relative_path: "src/invalid.rs".into(),
        bytes: vec![0xff],
    });
    assert_eq!(invalid.diagnostic, FileDiagnostic::NonUtf8);
    assert!(invalid.nodes.is_empty());
}

#[test]
fn parse_error_recovery_equals_clean_rebuild_and_retains_valid_identity() {
    let broken = source("src/lib.rs", "fn good() {}\nfn broken( {\n");
    let corrected = source("src/lib.rs", "fn good() {}\nfn broken() {}\n");
    let mut cache = ExtractionCache::new();
    let partial = cache.update(vec![broken]).unwrap();
    assert!(matches!(
        partial.facts[0].diagnostic,
        FileDiagnostic::ParseErrors { .. }
    ));
    let good_key = partial.facts[0].nodes[0].key.clone();
    let temp = tempfile::tempdir().unwrap();
    let mut store = CodegraphStore::open(temp.path().join("graph.sqlite"), Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let partial_file = &partial.facts[0];
    let run = store
        .begin_staged(
            &scope,
            &manifest(),
            &[SourceVersion {
                relative_path: partial_file.file.relative_path.clone(),
                source_digest: blake3::hash(&partial_file.file.bytes).to_hex().to_string(),
            }],
        )
        .unwrap();
    store.stage_file(&run, partial_file).unwrap();
    store.publish_staged(&run, PublishFault::None).unwrap();
    assert_eq!(
        store
            .current_completeness(scope.workspace_id())
            .unwrap()
            .degraded_files,
        1
    );
    assert!(
        store
            .search_name(scope.workspace_id(), "good", 2)
            .unwrap()
            .iter()
            .any(|node| node.name == "good" && node.span.start_line == 1)
    );
    let recovered = cache.update(vec![corrected.clone()]).unwrap();
    let clean = ExtractionCache::new().update(vec![corrected]).unwrap();
    assert_eq!(recovered.facts, clean.facts);
    assert_eq!(recovered.inventory.digest, clean.inventory.digest);
    assert!(
        recovered.facts[0]
            .nodes
            .iter()
            .any(|node| node.key == good_key)
    );
}

#[test]
fn cache_ingress_rejects_hard_oversize_and_parser_threshold_degrades() {
    let mut cache = ExtractionCache::new();
    let hard = rsi_codegraph::staged::MAX_STAGED_FILE_BYTES;
    assert!(
        cache
            .update(vec![SourceFile {
                relative_path: "src/huge.rs".into(),
                bytes: vec![b' '; hard + 1],
            }])
            .is_err()
    );
    let parser = rsi_codegraph::staged::PARSER_MAX_FILE_BYTES;
    let update = cache
        .update(vec![SourceFile {
            relative_path: "src/huge.rs".into(),
            bytes: vec![b' '; parser + 1],
        }])
        .unwrap();
    assert_eq!(
        update.facts[0].diagnostic,
        FileDiagnostic::Oversize { bytes: parser + 1 }
    );
    assert_eq!(update.inventory.owners.len(), 1);
}

#[test]
fn markdown_and_cargo_markers_have_source_spans() {
    let markdown = extract_file(source(
        "notes/report.md",
        "---\ntitle: X\n---\n# Report\n## Stage contract\n## F-001 Coverage\nsatisfies: [F-001]\nRationale: explicit\n",
    ));
    assert_eq!(markdown.diagnostic, FileDiagnostic::Parsed);
    assert!(
        markdown
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::MarkdownDocument)
    );
    assert!(
        markdown
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::StageContract)
    );
    assert!(
        markdown
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::ResearchFinding)
    );
    assert!(
        markdown
            .unresolved_references
            .iter()
            .any(|reference| reference.raw_target == "F-001")
    );
    let cargo = extract_file(source(
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[dependencies]\nserde = \"1\"\n",
    ));
    assert_eq!(cargo.diagnostic, FileDiagnostic::Parsed);
    assert!(
        cargo
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::Crate && node.name == "fixture")
    );
    assert!(
        cargo
            .unresolved_references
            .iter()
            .any(|reference| reference.raw_target == "serde")
    );
    assert_eq!(
        cargo
            .nodes
            .iter()
            .find(|node| node.kind == NodeKind::Crate)
            .unwrap()
            .span
            .start_line,
        2
    );
}

#[test]
fn markdown_fences_keep_real_findings_and_references_precise() {
    let text = "# Real\n```md\n## F-999 Fake\nRationale: fake F-998\n```\n## F-001 Actual\nRationale: real F-001\n`F-995 Rationale:`\n~~~\n# Stage contract\nF-997\n~~~\n\n    F-996\n";
    let facts = extract_file(source("notes/fences.md", text));
    assert_eq!(facts.diagnostic, FileDiagnostic::Parsed);
    let finding = facts
        .nodes
        .iter()
        .find(|node| node.kind == NodeKind::ResearchFinding)
        .unwrap();
    assert_eq!(finding.name, "F-001 Actual");
    assert_eq!(finding.span.start_byte, text.find("F-001 Actual").unwrap());
    assert_eq!(
        facts
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::RationaleMarker)
            .count(),
        1
    );
    assert_eq!(
        facts
            .unresolved_references
            .iter()
            .map(|reference| reference.raw_target.as_str())
            .collect::<Vec<_>>(),
        ["F-001", "F-001"]
    );
}

#[test]
fn cargo_syntax_context_and_name_value_spans_are_exact() {
    let text = "[package]\nname = \"name\"\ndescription = \"\"\"\n[dependencies]\nfake = \"1\"\n\"\"\"\n[dependencies]\nreal = \"1\"\n[[bin]]\nname = \"tool\"\npath = \"src/bin/tool.rs\"\n";
    let facts = extract_file(source("Cargo.toml", text));
    assert_eq!(facts.diagnostic, FileDiagnostic::Parsed);
    let package = facts
        .nodes
        .iter()
        .find(|node| node.kind == NodeKind::Crate)
        .unwrap();
    assert_eq!(package.name, "name");
    assert_eq!(
        &text[package.span.start_byte..package.span.end_byte],
        "name"
    );
    assert_eq!(package.span.start_byte, text.find("\"name\"").unwrap() + 1);
    let target = facts
        .nodes
        .iter()
        .find(|node| node.kind == NodeKind::CargoTarget)
        .unwrap();
    assert_eq!(target.name, "tool");
    assert_eq!(&text[target.span.start_byte..target.span.end_byte], "tool");
    assert_eq!(
        facts
            .unresolved_references
            .iter()
            .map(|reference| reference.raw_target.as_str())
            .collect::<Vec<_>>(),
        ["real"]
    );
}

#[test]
fn resolver_promotes_only_unique_explicit_crate_paths() {
    let mut files = vec![
        extract_file(source(
            "src/lib.rs",
            "fn caller() { crate::other::target(); target(); }\nuse crate::other::target;\n",
        )),
        extract_file(source("src/other.rs", "pub fn target() {}\n")),
    ];
    resolve_unique(&mut files);
    assert_eq!(
        files[0]
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Calls)
            .count(),
        1
    );
    assert_eq!(
        files[0]
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Imports)
            .count(),
        1
    );
    assert!(
        files[0]
            .unresolved_references
            .iter()
            .any(|reference| reference.raw_target == "target")
    );

    let temp = tempfile::tempdir().unwrap();
    let mut store = CodegraphStore::open(temp.path().join("graph.sqlite"), Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let versions = files
        .iter()
        .map(|facts| SourceVersion {
            relative_path: facts.file.relative_path.clone(),
            source_digest: blake3::hash(&facts.file.bytes).to_hex().to_string(),
        })
        .collect::<Vec<_>>();
    let run = store.begin_staged(&scope, &manifest(), &versions).unwrap();
    for facts in &files {
        store.stage_file(&run, facts).unwrap();
    }
    assert_eq!(
        store
            .publish_staged(&run, PublishFault::None)
            .unwrap()
            .generation,
        1
    );

    let mut ambiguous = vec![
        extract_file(source(
            "src/lib.rs",
            "fn caller() { crate::other::target(); }",
        )),
        extract_file(source("src/other.rs", "fn target() {} fn target() {}")),
    ];
    resolve_unique(&mut ambiguous);
    assert!(
        ambiguous[0]
            .unresolved_references
            .iter()
            .any(|reference| reference.raw_target == "crate::other::target")
    );
    assert_eq!(
        ambiguous[0]
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Calls)
            .count(),
        0
    );
}

#[test]
fn rust_tests_types_and_trait_impls_have_exact_proven_sites() {
    let text = "mod api { pub trait Trait {} pub struct Item; }\nimpl crate::api::Trait for crate::api::Item {}\nfn use_item(_: crate::api::Item) {}\n#[test]\nfn verifies() {}\n";
    let mut facts = vec![extract_file(source("src/lib.rs", text))];
    assert_eq!(facts[0].diagnostic, FileDiagnostic::Parsed);
    assert!(
        facts[0]
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::Test && node.name == "verifies")
    );
    assert!(
        facts[0]
            .unresolved_references
            .iter()
            .any(|reference| reference.raw_target == "crate::api::Trait")
    );
    resolve_unique(&mut facts);
    let implemented = facts[0]
        .relations
        .iter()
        .find(|relation| relation.kind == RelationKind::Implements)
        .unwrap();
    assert_eq!(
        &text[implemented.evidence[0].span.start_byte..implemented.evidence[0].span.end_byte],
        "crate::api::Trait"
    );
    assert!(
        facts[0]
            .relations
            .iter()
            .any(|relation| relation.kind == RelationKind::UsesType)
    );
}

#[test]
fn incremental_mutations_match_clean_rebuild_and_invalidate_dependents() {
    let mut cache = ExtractionCache::new();
    let base = vec![
        source(
            "src/lib.rs",
            "fn caller() { crate::b::target(); crate::b::target(); }\n",
        ),
        source("src/b.rs", "pub fn target() {}\n"),
        source(
            "notes/plan.md",
            "---\ntitle: one\n---\n# Plan\nsatisfies: [F-001]\n",
        ),
        source("notes/findings.md", "# Findings\n## F-001 Evidence\n"),
    ];
    let first = cache.update(base.clone()).unwrap();
    assert_eq!(
        first
            .facts
            .iter()
            .find(|file| file.file.relative_path == "notes/plan.md")
            .unwrap()
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::ReferencesFinding)
            .count(),
        1
    );
    assert!(first.inventory.dependencies["src/lib.rs"].contains("src/b.rs"));
    assert!(first.inventory.dependencies["notes/plan.md"].contains("notes/findings.md"));
    let unchanged = cache.update(base.clone()).unwrap();
    assert!(unchanged.changed_owners.is_empty());
    assert_eq!(first.inventory.digest, unchanged.inventory.digest);

    let changes = [
        vec![
            source("src/lib.rs", "fn caller() { crate::b::target(); }\n"),
            base[1].clone(),
            base[2].clone(),
            base[3].clone(),
        ],
        vec![base[0].clone(), base[2].clone(), base[3].clone()],
        vec![
            source("src/lib.rs", "fn caller( {\n"),
            base[1].clone(),
            base[2].clone(),
            base[3].clone(),
        ],
        vec![
            base[0].clone(),
            base[1].clone(),
            source(
                "notes/plan.md",
                "---\ntitle: two\n---\n# Plan\nsatisfies: [F-001]\n",
            ),
            base[3].clone(),
        ],
        vec![
            base[0].clone(),
            base[1].clone(),
            base[2].clone(),
            source("notes/findings.md", "# Findings\n## F-002 Evidence\n"),
        ],
    ];
    for files in changes {
        let update = cache.update(files.clone()).unwrap();
        let clean = ExtractionCache::new().update(files).unwrap();
        assert_eq!(update.facts, clean.facts);
        assert_eq!(update.inventory.digest, clean.inventory.digest);
    }
    let deleted = cache
        .update(vec![base[0].clone(), base[2].clone(), base[3].clone()])
        .unwrap();
    assert!(deleted.changed_owners.contains("src/b.rs"));
    assert!(deleted.invalidated_owners.contains("src/lib.rs"));
    assert_eq!(
        deleted
            .facts
            .iter()
            .flat_map(|file| &file.relations)
            .filter(|relation| relation.kind == RelationKind::Calls)
            .count(),
        0
    );
}
