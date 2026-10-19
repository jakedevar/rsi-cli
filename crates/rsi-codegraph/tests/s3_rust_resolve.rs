#![allow(clippy::expect_used, clippy::unwrap_used)] // Exact-source fixtures.

//! Rust reference resolution: imports, `Self`, impl methods, macro arguments,
//! test attributes and the Extracted/Inferred tiers (#2244).

use std::collections::HashMap;

use rsi_codegraph::{
    CodegraphStore, ExtractionContract, ExtractionMode, ExtractorIdentity, FactKey, FactProvenance,
    NodeKind, PerFileFacts, PublishFault, RelationKind, SourceFile, SourceVersion,
    StagedExtraction, WorkspaceInstanceKey,
    invalidate::ExtractionCache,
    query::{Direction, ProvenanceMode, QueryFilter, QueryLimits, SearchMode, SnapshotSelector},
};
use uuid::Uuid;

fn source(path: &str, text: &str) -> SourceFile {
    SourceFile {
        relative_path: path.into(),
        bytes: text.as_bytes().to_vec(),
    }
}

fn extract_all(files: &[(&str, &str)]) -> Vec<PerFileFacts> {
    let sources = files
        .iter()
        .map(|(path, text)| source(path, text))
        .collect::<Vec<_>>();
    ExtractionCache::new().update(sources).unwrap().facts
}

fn labels(facts: &[PerFileFacts]) -> HashMap<FactKey, String> {
    facts
        .iter()
        .flat_map(|file| &file.nodes)
        .map(|node| {
            (
                node.key.clone(),
                format!("{:?}:{}", node.kind, node.identity.qualified_name),
            )
        })
        .collect()
}

/// `source -> target` for every relation of `kind` with `provenance`, sorted.
fn edges(facts: &[PerFileFacts], kind: RelationKind, provenance: FactProvenance) -> Vec<String> {
    let names = labels(facts);
    let mut edges = facts
        .iter()
        .flat_map(|file| &file.relations)
        .filter(|relation| relation.kind == kind && relation.provenance == provenance)
        .map(|relation| format!("{} -> {}", names[&relation.source], names[&relation.target]))
        .collect::<Vec<_>>();
    edges.sort();
    edges
}

fn strings(items: &[&str]) -> Vec<String> {
    let mut items = items
        .iter()
        .map(|item| (*item).to_owned())
        .collect::<Vec<_>>();
    items.sort();
    items
}

fn manifest(mode: ExtractionMode) -> StagedExtraction {
    StagedExtraction {
        extraction: ExtractionContract {
            mode,
            extractor: ExtractorIdentity {
                name: "s3-resolve".into(),
                version: "1".into(),
            },
        },
        grammar_version: "tree-sitter-1".into(),
        rule_version: "s3-1".into(),
        normalization_version: "s3-1".into(),
        config_digest: "fixture".into(),
    }
}

fn versions(facts: &[PerFileFacts]) -> Vec<SourceVersion> {
    facts
        .iter()
        .map(|file| SourceVersion {
            relative_path: file.file.relative_path.clone(),
            source_digest: blake3::hash(&file.file.bytes).to_hex().to_string(),
        })
        .collect()
}

#[test]
fn imports_aliases_groups_globs_and_module_paths_resolve() {
    let facts = extract_all(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        (
            "src/lib.rs",
            "pub mod util;\npub mod model;\nmod inner { pub fn deep() {} pub mod nested { pub fn leaf() {} } }\nuse util::{helper as renamed, other};\nuse inner::nested::leaf;\nuse model::*;\npub fn root() {\n    renamed();\n    other();\n    leaf();\n    make();\n    inner::deep();\n    crate::util::helper();\n}\n",
        ),
        (
            "src/util.rs",
            "pub fn helper() {}\npub fn other() { super::root(); self::helper(); }\n",
        ),
        ("src/model.rs", "pub fn make() {}\n"),
    ]);
    assert_eq!(
        edges(&facts, RelationKind::Calls, FactProvenance::Extracted),
        strings(&[
            "Function:src/lib.rs::root -> Function:src/lib.rs::inner::deep",
            "Function:src/lib.rs::root -> Function:src/lib.rs::inner::nested::leaf",
            "Function:src/lib.rs::root -> Function:src/model.rs::make",
            "Function:src/lib.rs::root -> Function:src/util.rs::helper",
            "Function:src/lib.rs::root -> Function:src/util.rs::helper",
            "Function:src/lib.rs::root -> Function:src/util.rs::other",
            "Function:src/util.rs::other -> Function:src/lib.rs::root",
            "Function:src/util.rs::other -> Function:src/util.rs::helper",
        ])
    );
    // Every use leaf names a module or an item, so the declarations are proven
    // and no import site is left unresolved in the library root.
    let imports = edges(&facts, RelationKind::Imports, FactProvenance::Extracted);
    assert_eq!(
        imports.len(),
        4,
        "alias, group, nested and glob imports all resolve: {imports:?}"
    );
    let lib = facts
        .iter()
        .find(|file| file.file.relative_path == "src/lib.rs")
        .unwrap();
    assert!(
        lib.unresolved_references
            .iter()
            .all(|reference| reference.kind != rsi_codegraph::UnresolvedReferenceKind::Import),
        "unresolved: {:?}",
        lib.unresolved_references
    );
}

#[test]
fn mod_files_and_path_attributes_build_the_module_tree() {
    let facts = extract_all(&[
        (
            "src/lib.rs",
            "mod a;\n#[path = \"custom/z.rs\"]\nmod z;\npub fn r() { a::b::deep(); z::zed(); }\n",
        ),
        ("src/a.rs", "pub mod b;\n"),
        ("src/a/b.rs", "pub fn deep() {}\n"),
        ("src/custom/z.rs", "pub fn zed() {}\n"),
    ]);
    assert_eq!(
        edges(&facts, RelationKind::Calls, FactProvenance::Extracted),
        strings(&[
            "Function:src/lib.rs::r -> Function:src/a/b.rs::deep",
            "Function:src/lib.rs::r -> Function:src/custom/z.rs::zed",
        ])
    );
}

#[test]
fn workspace_crates_resolve_by_name() {
    let facts = extract_all(&[
        ("crates/a/Cargo.toml", "[package]\nname = \"alpha\"\n"),
        ("crates/a/src/lib.rs", "pub fn shout() {}\n"),
        ("crates/b/Cargo.toml", "[package]\nname = \"beta\"\n"),
        (
            "crates/b/src/lib.rs",
            "use alpha::shout;\npub fn go() { shout(); alpha::shout(); }\n",
        ),
    ]);
    assert_eq!(
        edges(&facts, RelationKind::Calls, FactProvenance::Extracted),
        strings(&[
            "Function:crates/b/src/lib.rs::go -> Function:crates/a/src/lib.rs::shout",
            "Function:crates/b/src/lib.rs::go -> Function:crates/a/src/lib.rs::shout",
        ])
    );
}

#[test]
fn self_assoc_and_method_calls_resolve_inside_impls() {
    let facts = extract_all(&[(
        "src/lib.rs",
        "pub struct Store;\nimpl Store {\n    pub fn new() -> Self { Store }\n    pub fn put(&self) {}\n}\npub struct Engine { store: Store }\nimpl Engine {\n    pub fn build() -> Self {\n        let s = Store::new();\n        s.put();\n        Engine { store: s }\n    }\n    pub fn run(&self) {\n        self.step();\n        Self::helper();\n        self.store.put();\n    }\n    fn step(&self) {}\n    fn helper() {}\n}\npub fn external(e: &Engine, other: Engine) {\n    e.run();\n    other.run();\n}\npub fn shadowed(e: &Engine) {\n    let e = 5;\n    e.run();\n}\n",
    )]);
    assert_eq!(
        edges(&facts, RelationKind::Calls, FactProvenance::Extracted),
        strings(&[
            "Method:src/lib.rs::Engine::build -> Method:src/lib.rs::Store::new",
            "Method:src/lib.rs::Engine::build -> Method:src/lib.rs::Store::put",
            "Method:src/lib.rs::Engine::run -> Method:src/lib.rs::Engine::helper",
            "Method:src/lib.rs::Engine::run -> Method:src/lib.rs::Engine::step",
            "Method:src/lib.rs::Engine::run -> Method:src/lib.rs::Store::put",
            "Function:src/lib.rs::external -> Method:src/lib.rs::Engine::run",
            "Function:src/lib.rs::external -> Method:src/lib.rs::Engine::run",
        ])
    );
    // `shadowed` rebinds `e`, so its receiver type is not proven; nothing is
    // guessed for the unknown receiver.
    assert!(
        edges(&facts, RelationKind::Calls, FactProvenance::Inferred).is_empty(),
        "no inferred edges expected"
    );
}

#[test]
fn trait_impls_become_implements_edges_and_trait_methods_resolve() {
    let facts = extract_all(&[(
        "src/lib.rs",
        "pub trait Shape {\n    fn area(&self) -> u32;\n    fn describe(&self) -> u32 { 0 }\n}\npub struct Sq;\nimpl Shape for Sq {\n    fn area(&self) -> u32 { 1 }\n}\npub struct Other;\nimpl std::fmt::Display for Other {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }\n}\npub fn use_it(s: &Sq) -> u32 { s.area() + s.describe() }\n",
    )]);
    assert_eq!(
        edges(&facts, RelationKind::Implements, FactProvenance::Extracted),
        strings(&["Impl:src/lib.rs::Sq -> Trait:src/lib.rs::Shape"])
    );
    assert_eq!(
        edges(&facts, RelationKind::Calls, FactProvenance::Extracted),
        strings(&[
            "Function:src/lib.rs::use_it -> Method:src/lib.rs::Sq::area",
            "Function:src/lib.rs::use_it -> Function:src/lib.rs::Shape::describe",
        ])
    );
}

#[test]
fn macro_arguments_yield_calls_with_exact_spans() {
    let text = "fn f() -> u32 { 1 }\nfn g(x: u32) -> u32 { x }\n#[test]\nfn t() {\n    assert_eq!(f(), g(f()));\n    assert!(matches!(Some(f()), Some(_)));\n    let v = vec![g(1); 3];\n    println!(\"{}\", g(2));\n    let _ = format!(\"{}\", f());\n    let j = serde_json::json!({\"a\": f(), \"k\": [g(2)]});\n}\n";
    let facts = extract_all(&[("src/lib.rs", text)]);
    assert_eq!(
        edges(&facts, RelationKind::Calls, FactProvenance::Extracted),
        strings(&[
            "Test:src/lib.rs::t -> Function:src/lib.rs::f",
            "Test:src/lib.rs::t -> Function:src/lib.rs::f",
            "Test:src/lib.rs::t -> Function:src/lib.rs::f",
            "Test:src/lib.rs::t -> Function:src/lib.rs::f",
            "Test:src/lib.rs::t -> Function:src/lib.rs::f",
            "Test:src/lib.rs::t -> Function:src/lib.rs::g",
            "Test:src/lib.rs::t -> Function:src/lib.rs::g",
            "Test:src/lib.rs::t -> Function:src/lib.rs::g",
            "Test:src/lib.rs::t -> Function:src/lib.rs::g",
        ])
    );
    // Each macro-derived site points at the callee text in the original file.
    for relation in facts.iter().flat_map(|file| &file.relations) {
        if relation.kind != RelationKind::Calls {
            continue;
        }
        let span = &relation.evidence[0].span;
        let slice = &text[span.start_byte..span.end_byte];
        assert!(slice == "f" || slice == "g", "span text {slice:?}");
        let line = text[..span.start_byte].matches('\n').count() + 1;
        assert_eq!(span.start_line, line);
    }
}

#[test]
fn every_test_attribute_form_marks_a_test() {
    let text = "#[tokio::test(flavor = \"multi_thread\")]\nasync fn a() {}\n#[sqlx::test]\nasync fn b() {}\n#[rstest]\nfn c() {}\n#[test]\nfn d() {}\n#[cfg(test)]\nfn not_test() {}\n#[tokio::main]\nasync fn main_not_test() {}\n/// doc\n#[tokio::test]\n/// trailing doc\nasync fn e() {}\n";
    let facts = extract_all(&[("src/lib.rs", text)]);
    let kind_of = |name: &str| {
        facts[0]
            .nodes
            .iter()
            .find(|node| node.name == name)
            .map(|node| node.kind)
    };
    for name in ["a", "b", "c", "d", "e"] {
        assert_eq!(kind_of(name), Some(NodeKind::Test), "{name}");
    }
    for name in ["not_test", "main_not_test"] {
        assert_eq!(kind_of(name), Some(NodeKind::Function), "{name}");
    }
}

const INFERRED_SOURCE: &str = "pub struct A;\nimpl A { pub fn compute_total(&self) {} }\npub struct B;\nimpl B { pub fn compute_total(&self) {} }\npub struct C;\nimpl C { pub fn unique_thing(&self) {} }\npub fn make() -> A { A }\npub fn caller() {\n    make().compute_total();\n    make().unique_thing();\n    make().len();\n}\n";

#[test]
fn unknown_receivers_get_bounded_inferred_candidates_never_extracted() {
    let facts = extract_all(&[("src/lib.rs", INFERRED_SOURCE)]);
    assert_eq!(
        edges(&facts, RelationKind::Calls, FactProvenance::Extracted),
        strings(&[
            "Function:src/lib.rs::caller -> Function:src/lib.rs::make",
            "Function:src/lib.rs::caller -> Function:src/lib.rs::make",
            "Function:src/lib.rs::caller -> Function:src/lib.rs::make",
        ])
    );
    assert_eq!(
        edges(&facts, RelationKind::Calls, FactProvenance::Inferred),
        strings(&[
            "Function:src/lib.rs::caller -> Method:src/lib.rs::A::compute_total",
            "Function:src/lib.rs::caller -> Method:src/lib.rs::B::compute_total",
            "Function:src/lib.rs::caller -> Method:src/lib.rs::C::unique_thing",
        ])
    );
    // A guessed site stays listed as unresolved; only a proof removes it.
    let unresolved = facts[0]
        .unresolved_references
        .iter()
        .map(|reference| reference.raw_target.as_str())
        .collect::<Vec<_>>();
    assert!(
        unresolved.contains(&"make().unique_thing"),
        "{unresolved:?}"
    );
    assert!(unresolved.contains(&"make().len"), "{unresolved:?}");
    for relation in facts[0]
        .relations
        .iter()
        .filter(|relation| relation.provenance == FactProvenance::Inferred)
    {
        assert!(
            relation.evidence[0].label.starts_with("inferred:"),
            "{}",
            relation.evidence[0].label
        );
    }
}

#[test]
fn strict_queries_hide_inferred_edges_and_exploratory_shows_them() {
    let facts = extract_all(&[("src/lib.rs", INFERRED_SOURCE)]);
    let temp = tempfile::tempdir().unwrap();
    let mut store = CodegraphStore::open(temp.path().join("graph.sqlite"), Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let run = store
        .begin_staged(
            &scope,
            &manifest(ExtractionMode::ExtractedInferredV1_1),
            &versions(&facts),
        )
        .unwrap();
    for file in &facts {
        store.stage_file(&run, file).unwrap();
    }
    let ready = store.publish_staged(&run, PublishFault::None).unwrap();
    assert_eq!(ready.extraction.mode, ExtractionMode::ExtractedInferredV1_1);
    let query = store
        .query(scope.workspace_id(), SnapshotSelector::CurrentReady)
        .unwrap();
    let strict = QueryFilter::default();
    let exploratory = QueryFilter {
        provenance: ProvenanceMode::Exploratory,
        relation_kinds: vec![RelationKind::Calls],
    };
    let target = query
        .search(
            SearchMode::ExactName,
            "unique_thing",
            &exploratory,
            QueryLimits::default(),
        )
        .unwrap()
        .value
        .into_iter()
        .next()
        .unwrap();
    let strict_callers = query
        .neighbors(
            target.id,
            Direction::Incoming,
            &QueryFilter {
                relation_kinds: vec![RelationKind::Calls],
                ..strict
            },
            QueryLimits::default(),
        )
        .unwrap();
    assert!(strict_callers.value.relations.is_empty());
    let inferred_callers = query
        .neighbors(
            target.id,
            Direction::Incoming,
            &exploratory,
            QueryLimits::default(),
        )
        .unwrap();
    assert_eq!(inferred_callers.value.relations.len(), 1);
    assert_eq!(
        inferred_callers.value.relations[0].provenance,
        FactProvenance::Inferred
    );
}

#[test]
fn the_strict_extraction_mode_still_refuses_inferred_relations() {
    let facts = extract_all(&[("src/lib.rs", INFERRED_SOURCE)]);
    assert!(
        facts[0]
            .relations
            .iter()
            .any(|relation| relation.provenance == FactProvenance::Inferred)
    );
    let temp = tempfile::tempdir().unwrap();
    let mut store = CodegraphStore::open(temp.path().join("graph.sqlite"), Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let run = store
        .begin_staged(
            &scope,
            &manifest(ExtractionMode::ExtractedV1_0),
            &versions(&facts),
        )
        .unwrap();
    assert!(store.stage_file(&run, &facts[0]).is_err());
}
