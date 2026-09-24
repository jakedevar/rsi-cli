#![allow(clippy::expect_used, clippy::unwrap_used)] // Temporary Cargo fixture.

use std::{fs, path::Path, process::Command};

use rsi_codegraph::{
    CodegraphStore, ExtractionContract, ExtractionMode, ExtractorIdentity, PublishFault,
    RelationKind, SourceFile, SourceVersion, StagedExtraction, WorkspaceInstanceKey,
    cargo_metadata::{apply_workspace, read_workspace},
    extract::extract_file,
    invalidate::ExtractionCache,
};
use uuid::Uuid;

fn write(root: &Path, path: &str, content: &str) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, content).unwrap();
}

fn manifest() -> StagedExtraction {
    StagedExtraction {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "s1-cargo-fixture".into(),
                version: "1".into(),
            },
        },
        grammar_version: "tree-sitter-toml-1".into(),
        rule_version: "cargo-metadata-1".into(),
        normalization_version: "1".into(),
        config_digest: "fixture".into(),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // Fixture, metadata proof, and staged publication form one check.
fn cargo_metadata_resolves_workspace_package_target_and_exact_dependency_sites() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"core\"]\nresolver = \"2\"\n",
    );
    write(
        root,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\ncore_alias = { package = \"core\", path = \"../core\" }\n[dev-dependencies]\ncore_alias = { package = \"core\", path = \"../core\" }\n",
    );
    write(
        root,
        "app/src/lib.rs",
        "pub fn app() { core_alias::core(); }\n",
    );
    write(
        root,
        "core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "core/src/lib.rs", "pub fn core() {}\n");
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    let metadata = read_workspace(root).unwrap();
    assert_eq!(metadata.packages.len(), 2);
    assert_eq!(metadata.dependencies.len(), 2);
    let app = metadata
        .packages
        .iter()
        .find(|package| package.name == "app")
        .unwrap();
    assert!(
        app.targets
            .iter()
            .any(|target| target.name == "app" && target.source_path == "app/src/lib.rs")
    );
    let normal = metadata
        .dependencies
        .iter()
        .find(|dependency| dependency.kind == RelationKind::DependsOn)
        .unwrap();
    assert_eq!(normal.kind, RelationKind::DependsOn);
    assert_eq!(normal.package_name, "core");
    assert_eq!(normal.manifest_span.as_ref().unwrap().start_line, 6);
    let dev = metadata
        .dependencies
        .iter()
        .find(|dependency| dependency.kind == RelationKind::DevDependsOn)
        .unwrap();
    assert_eq!(dev.kind, RelationKind::DevDependsOn);
    assert_eq!(dev.manifest_span.as_ref().unwrap().start_line, 8);

    let mut facts = ["Cargo.toml", "app/Cargo.toml", "core/Cargo.toml"]
        .into_iter()
        .map(|path| {
            extract_file(SourceFile {
                relative_path: path.into(),
                bytes: fs::read(root.join(path)).unwrap(),
            })
        })
        .collect::<Vec<_>>();
    apply_workspace(&mut facts, &metadata);
    let app_facts = facts
        .iter()
        .find(|facts| facts.file.relative_path == "app/Cargo.toml")
        .unwrap();
    assert!(
        app_facts
            .nodes
            .iter()
            .any(|node| node.kind == rsi_codegraph::NodeKind::CargoTarget && node.name == "app")
    );
    assert_eq!(
        app_facts
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::DependsOn)
            .count(),
        1
    );
    assert_eq!(
        app_facts
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::DevDependsOn)
            .count(),
        1
    );
    assert!(app_facts.unresolved_references.is_empty());

    let mut store = CodegraphStore::open(root.join("graph.sqlite"), Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let versions = facts
        .iter()
        .map(|facts| SourceVersion {
            relative_path: facts.file.relative_path.clone(),
            source_digest: blake3::hash(&facts.file.bytes).to_hex().to_string(),
        })
        .collect::<Vec<_>>();
    let run = store.begin_staged(&scope, &manifest(), &versions).unwrap();
    for file in &facts {
        store.stage_file(&run, file).unwrap();
    }
    assert_eq!(
        store
            .publish_staged(&run, PublishFault::None)
            .unwrap()
            .generation,
        1
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Workspace metadata, invalidation, and publication share one fixture.
fn current_workspace_dotted_dependencies_map_to_exact_manifest_keys() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let metadata = read_workspace(root).unwrap();
    let codegraph = metadata
        .packages
        .iter()
        .find(|package| package.name == "rsi-codegraph")
        .unwrap();
    let blake3 = metadata
        .dependencies
        .iter()
        .find(|dependency| {
            dependency.owner_package_id == codegraph.id
                && dependency.alias == "blake3"
                && dependency.kind == RelationKind::DependsOn
        })
        .unwrap();
    let span = blake3.manifest_span.as_ref().unwrap();
    assert_eq!(span.path, "crates/rsi-codegraph/Cargo.toml");
    let source = fs::read(root.join(&span.path)).unwrap();
    assert_eq!(&source[span.start_byte..span.end_byte], b"blake3");
    let missing = metadata
        .dependencies
        .iter()
        .filter(|dependency| dependency.manifest_span.is_none())
        .map(|dependency| {
            format!(
                "{}:{}:{:?}",
                dependency.owner_package_id, dependency.alias, dependency.kind
            )
        })
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "Cargo metadata lacks exact manifest sites: {missing:?}"
    );
    let mut facts = metadata
        .packages
        .iter()
        .map(|package| {
            extract_file(SourceFile {
                relative_path: package.manifest_path.clone(),
                bytes: fs::read(root.join(&package.manifest_path)).unwrap(),
            })
        })
        .collect::<Vec<_>>();
    apply_workspace(&mut facts, &metadata);
    let cargo_relations = facts
        .iter()
        .flat_map(|file| &file.relations)
        .filter(|relation| {
            matches!(
                relation.kind,
                RelationKind::DependsOn | RelationKind::DevDependsOn | RelationKind::BuildDependsOn
            )
        })
        .count();
    assert_eq!(cargo_relations, metadata.dependencies.len());
    let sources = metadata
        .packages
        .iter()
        .map(|package| SourceFile {
            relative_path: package.manifest_path.clone(),
            bytes: fs::read(root.join(&package.manifest_path)).unwrap(),
        })
        .collect::<Vec<_>>();
    let mut cache = ExtractionCache::new();
    let update = cache.update_with_cargo(sources.clone(), &metadata).unwrap();
    assert!(
        update.inventory.dependencies["crates/rsid/Cargo.toml"]
            .contains("crates/rsi-common/Cargo.toml")
    );
    let unchanged = cache.update_with_cargo(sources.clone(), &metadata).unwrap();
    assert!(unchanged.changed_owners.is_empty());
    let mut changed_metadata = metadata.clone();
    changed_metadata.metadata_digest = "changed-lockfile-metadata".into();
    let changed = cache.update_with_cargo(sources, &changed_metadata).unwrap();
    assert!(changed.changed_owners.contains("crates/rsid/Cargo.toml"));
    assert_ne!(update.inventory.digest, changed.inventory.digest);
    let temp = tempfile::tempdir().unwrap();
    let mut store = CodegraphStore::open(temp.path().join("graph.sqlite"), Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let versions = facts
        .iter()
        .map(|facts| SourceVersion {
            relative_path: facts.file.relative_path.clone(),
            source_digest: blake3::hash(&facts.file.bytes).to_hex().to_string(),
        })
        .collect::<Vec<_>>();
    let run = store.begin_staged(&scope, &manifest(), &versions).unwrap();
    for file in &facts {
        store.stage_file(&run, file).unwrap();
    }
    assert_eq!(
        store
            .publish_staged(&run, PublishFault::None)
            .unwrap()
            .generation,
        1
    );
}

#[test]
fn cargo_dependency_mutation_invalidates_owner_and_matches_clean_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"core\"]\nresolver = \"2\"\n",
    );
    let with_dependency = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\ndescription = \"\"\"\n[dependencies]\ncore = \"999\"\n\"\"\"\n[dependencies]\ncore = { path = \"../core\" }\n";
    let without_dependency = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    write(root, "app/Cargo.toml", with_dependency);
    write(root, "app/src/lib.rs", "pub fn app() {}\n");
    write(
        root,
        "core/Cargo.toml",
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(root, "core/src/lib.rs", "pub fn core() {}\n");
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    let sources = || {
        ["Cargo.toml", "app/Cargo.toml", "core/Cargo.toml"]
            .into_iter()
            .map(|path| SourceFile {
                relative_path: path.into(),
                bytes: fs::read(root.join(path)).unwrap(),
            })
            .collect::<Vec<_>>()
    };
    let mut cache = ExtractionCache::new();
    let metadata = read_workspace(root).unwrap();
    assert_eq!(metadata.dependencies.len(), 1);
    let span = metadata.dependencies[0].manifest_span.as_ref().unwrap();
    assert_eq!(
        &with_dependency.as_bytes()[span.start_byte..span.end_byte],
        b"core"
    );
    assert_eq!(span.start_byte, with_dependency.rfind("core = ").unwrap());
    let initial = cache.update_with_cargo(sources(), &metadata).unwrap();
    assert_eq!(
        initial
            .facts
            .iter()
            .flat_map(|file| &file.relations)
            .filter(|relation| relation.kind == RelationKind::DependsOn)
            .count(),
        1
    );
    write(root, "app/Cargo.toml", without_dependency);
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    let metadata = read_workspace(root).unwrap();
    assert!(metadata.dependencies.is_empty());
    let changed_sources = sources();
    let changed = cache
        .update_with_cargo(changed_sources.clone(), &metadata)
        .unwrap();
    let clean = ExtractionCache::new()
        .update_with_cargo(changed_sources, &metadata)
        .unwrap();
    assert!(changed.changed_owners.contains("app/Cargo.toml"));
    assert!(changed.invalidated_owners.contains("app/Cargo.toml"));
    assert_eq!(changed.facts, clean.facts);
    assert_eq!(changed.inventory.digest, clean.inventory.digest);
    assert_eq!(
        changed
            .facts
            .iter()
            .flat_map(|file| &file.relations)
            .filter(|relation| relation.kind == RelationKind::DependsOn)
            .count(),
        0
    );
}

#[test]
fn cargo_metadata_projection_is_independent_of_temporary_root() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for root in [first.path(), second.path()] {
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
        );
        write(root, "src/lib.rs", "pub fn fixture() {}\n");
        assert!(
            Command::new("cargo")
                .args(["generate-lockfile", "--offline"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
    assert_eq!(
        read_workspace(first.path()).unwrap(),
        read_workspace(second.path()).unwrap()
    );
}
