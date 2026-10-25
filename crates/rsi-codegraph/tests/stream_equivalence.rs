//! #3491: a streamed index pass yields exactly the facts the in-memory pass does.
//!
//! The streamed pass spills what the resolver does not read and loads it back
//! one file at a time; this pins that the final facts, in order, are identical
//! with and without Cargo metadata, and that the pass tracks changes.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Temporary workspace fixture.

use std::{fs, path::Path, process::Command};

use rsi_codegraph::{
    PerFileFacts, SourceFile, cargo_metadata::read_workspace, invalidate::ExtractionCache,
    scope::RustScope,
};

fn write(root: &Path, path: &str, content: &str) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, content).unwrap();
}

fn fixture(root: &Path) {
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"core\"]\nresolver = \"2\"\n",
    );
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\ncore_alias = { package = \"core\", path = \"../core\" }\n",
    );
    write(
        root,
        "app/src/lib.rs",
        "mod util;\nuse util::helper;\npub fn app() { core_alias::core(); helper(); local(); }\nfn local() { util::helper(); }\n",
    );
    write(
        root,
        "app/src/util.rs",
        "pub fn helper() { crate::app(); }\n",
    );
    write(
        root,
        "core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        root,
        "core/src/lib.rs",
        "pub fn core() {}\npub struct Thing;\nimpl Thing { pub fn go(&self) { core(); } }\n",
    );
    write(
        root,
        "notes/plan.md",
        "---\ntitle: plan\n---\n# Plan\nsatisfies: [F-001]\n\nSee [the code](app/src/lib.rs).\n",
    );
    write(root, "notes/findings.md", "# Findings\n## F-001 Evidence\n");
    write(root, "notes/broken.md", "# Broken\nsatisfies: [F-404]\n");
    write(root, "app/src/broken.rs", "pub fn broken( {\n");
}

fn read_sources(root: &Path) -> Vec<SourceFile> {
    let mut paths = [
        "Cargo.toml",
        "app/Cargo.toml",
        "app/src/broken.rs",
        "app/src/lib.rs",
        "app/src/util.rs",
        "core/Cargo.toml",
        "core/src/lib.rs",
        "notes/broken.md",
        "notes/findings.md",
        "notes/plan.md",
    ]
    .map(String::from)
    .to_vec();
    paths.sort();
    paths
        .into_iter()
        .map(|path| SourceFile {
            bytes: fs::read(root.join(&path)).unwrap(),
            relative_path: path,
        })
        .collect()
}

fn without_scope(mut facts: Vec<PerFileFacts>) -> Vec<PerFileFacts> {
    for file in &mut facts {
        file.scope = RustScope::default();
    }
    facts
}

#[test]
fn a_streamed_pass_yields_the_in_memory_facts_with_and_without_cargo() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    fixture(&root);
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    let metadata = read_workspace(&root).unwrap();
    let spill = temp.path().join("spill");
    fs::create_dir(&spill).unwrap();

    for cargo in [None, Some(&metadata)] {
        let mut memory = ExtractionCache::new();
        let in_memory = match cargo {
            Some(metadata) => memory
                .update_with_cargo(read_sources(&root), metadata)
                .unwrap(),
            None => memory.update(read_sources(&root)).unwrap(),
        };

        let mut streaming = ExtractionCache::with_retain_budget(0);
        let mut streamed = streaming
            .update_pass(read_sources(&root), cargo, &spill)
            .unwrap();
        assert!(streamed.facts.is_streamed());
        assert!(!streaming.retains_raw_facts());
        assert_eq!(
            streamed.changed_owners, in_memory.changed_owners,
            "a first pass changes every owner, with or without Cargo"
        );

        let streamed_facts = streamed.facts.collect().unwrap();
        let expected = without_scope(in_memory.facts);
        assert_eq!(streamed_facts.len(), expected.len());
        for (got, want) in streamed_facts.iter().zip(&expected) {
            assert_eq!(got, want, "{}", want.file.relative_path);
        }
        // Visiting twice (a staging retry) yields the same facts.
        assert_eq!(streamed.facts.collect().unwrap(), streamed_facts);
        // The streamed pass resolved something real, so the comparison has teeth.
        assert!(
            expected
                .iter()
                .flat_map(|file| &file.relations)
                .any(
                    |relation| relation.provenance == rsi_codegraph::FactProvenance::Extracted
                        && relation.kind == rsi_codegraph::RelationKind::ReferencesFinding
                ),
            "the fixture resolves a finding reference"
        );
        assert!(
            expected
                .iter()
                .any(|file| !file.unresolved_references.is_empty()),
            "the fixture keeps an unresolved reference"
        );

        // The digests drive change detection on the next pass.
        let again = streaming
            .update_pass(read_sources(&root), cargo, &spill)
            .unwrap();
        assert!(again.changed_owners.is_empty());
        write(&root, "notes/findings.md", "# Findings\n## F-001 Changed\n");
        let changed = streaming
            .update_pass(read_sources(&root), cargo, &spill)
            .unwrap();
        assert_eq!(
            changed.changed_owners.iter().cloned().collect::<Vec<_>>(),
            vec!["notes/findings.md".to_owned()]
        );
        write(
            &root,
            "notes/findings.md",
            "# Findings\n## F-001 Evidence\n",
        );
    }
}

#[test]
fn a_streamed_pass_rejects_a_duplicate_owner() {
    let temp = tempfile::tempdir().unwrap();
    let source = |path: &str| SourceFile {
        relative_path: path.into(),
        bytes: b"pub fn one() {}\n".to_vec(),
    };
    let mut cache = ExtractionCache::with_retain_budget(0);
    let result = cache.update_pass(
        vec![source("src/a.rs"), source("src/a.rs")],
        None,
        temp.path(),
    );
    assert!(result.is_err());
}
