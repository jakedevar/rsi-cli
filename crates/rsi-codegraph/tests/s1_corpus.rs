#![allow(clippy::expect_used, clippy::unwrap_used)] // Explicit corpus gate with primary output.

use std::{fs, path::Path, process::Command, time::Instant};

use rsi_codegraph::{
    CodegraphStore, ExtractionContract, ExtractionMode, ExtractorIdentity, NodeKind, PublishFault,
    SourceFile, SourceVersion, StagedExtraction, WorkspaceInstanceKey,
    cargo_metadata::read_workspace, invalidate::ExtractionCache,
};
use uuid::Uuid;

fn manifest(metadata_digest: &str) -> StagedExtraction {
    StagedExtraction {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedV1_0,
            extractor: ExtractorIdentity {
                name: "s1-corpus".into(),
                version: "1".into(),
            },
        },
        grammar_version: "tree-sitter-1".into(),
        rule_version: "s1-1".into(),
        normalization_version: "s1-1".into(),
        config_digest: metadata_digest.into(),
    }
}

#[test]
#[ignore = "full temporary RSI corpus; run explicitly for the S1 gate"]
#[allow(clippy::too_many_lines)] // The corpus capture, publication, and two-snapshot golden form one gate.
fn full_temporary_rsi_corpus_has_deterministic_ready_goldens() {
    let started = Instant::now();
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = crate_dir.parent().unwrap().parent().unwrap();
    let listing = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let supported_paths = listing
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).unwrap())
        .filter(|path| {
            let extension = Path::new(path).extension().and_then(|value| value.to_str());
            matches!(extension, Some("rs" | "md" | "toml"))
        })
        .collect::<Vec<_>>();
    let symlinks = supported_paths
        .iter()
        .filter(|path| {
            fs::symlink_metadata(repo.join(path))
                .unwrap()
                .file_type()
                .is_symlink()
        })
        .count();
    let paths = supported_paths
        .into_iter()
        .filter(|path| {
            !fs::symlink_metadata(repo.join(path))
                .unwrap()
                .file_type()
                .is_symlink()
        })
        .collect::<Vec<_>>();
    assert!(paths.len() > 3_950);
    assert_eq!(symlinks, 2);
    let temp = tempfile::tempdir().unwrap();
    eprintln!("S1 corpus temp root: {}", temp.path().display());
    let corpus = temp.path().join("rsi");
    fs::create_dir(&corpus).unwrap();
    fs::copy(repo.join("Cargo.lock"), corpus.join("Cargo.lock")).unwrap();
    let mut sources = Vec::with_capacity(paths.len());
    let mut counts = [0usize; 3];
    let mut source_bytes = 0usize;
    for path in &paths {
        let original = repo.join(path);
        assert!(original.is_file());
        let copied = corpus.join(path);
        fs::create_dir_all(copied.parent().unwrap()).unwrap();
        fs::copy(&original, &copied).unwrap();
        let bytes = fs::read(&copied).unwrap();
        source_bytes += bytes.len();
        match Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap()
        {
            "rs" => counts[0] += 1,
            "md" => counts[1] += 1,
            "toml" => counts[2] += 1,
            _ => unreachable!(),
        }
        sources.push(SourceFile {
            relative_path: path.clone(),
            bytes,
        });
    }
    let metadata = read_workspace(&corpus).unwrap();
    assert!(
        metadata
            .dependencies
            .iter()
            .all(|dependency| dependency.manifest_span.is_some())
    );
    let mut cache = ExtractionCache::new();
    let extracted = cache.update_with_cargo(sources.clone(), &metadata).unwrap();
    let parsed = extracted
        .facts
        .iter()
        .filter(|file| file.diagnostic == rsi_codegraph::FileDiagnostic::Parsed)
        .count();
    let diagnostics = extracted
        .facts
        .iter()
        .fold([0usize; 5], |mut counts, file| {
            let index = match &file.diagnostic {
                rsi_codegraph::FileDiagnostic::Parsed => 0,
                rsi_codegraph::FileDiagnostic::ParseErrors { .. } => 1,
                rsi_codegraph::FileDiagnostic::Unsupported { .. } => 2,
                rsi_codegraph::FileDiagnostic::Oversize { .. } => 3,
                rsi_codegraph::FileDiagnostic::NonUtf8 => 4,
            };
            counts[index] += 1;
            counts
        });
    let nodes = extracted
        .facts
        .iter()
        .map(|file| file.nodes.len())
        .sum::<usize>();
    let relations = extracted
        .facts
        .iter()
        .map(|file| file.relations.len())
        .sum::<usize>();
    let unresolved = extracted
        .facts
        .iter()
        .map(|file| file.unresolved_references.len())
        .sum::<usize>();
    eprintln!(
        "S1 corpus: files={} symlinks_excluded={} rust={} markdown={} toml={} bytes={} parsed={} degraded={} nodes={} relations={} unresolved={} cargo_dependencies={} exact_cargo_sites={} inventory_digest={}",
        paths.len(),
        symlinks,
        counts[0],
        counts[1],
        counts[2],
        source_bytes,
        parsed,
        paths.len() - parsed,
        nodes,
        relations,
        unresolved,
        metadata.dependencies.len(),
        metadata
            .dependencies
            .iter()
            .filter(|dependency| dependency.manifest_span.is_some())
            .count(),
        extracted.inventory.digest
    );
    assert_eq!(counts.iter().sum::<usize>(), paths.len());
    assert_eq!(extracted.inventory.owners.len(), paths.len());
    assert!(paths.iter().all(|path| {
        !Path::new(path)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
    }));
    let versions = extracted
        .facts
        .iter()
        .map(|facts| SourceVersion {
            relative_path: facts.file.relative_path.clone(),
            source_digest: blake3::hash(&facts.file.bytes).to_hex().to_string(),
        })
        .collect::<Vec<_>>();
    let mut store = CodegraphStore::open(temp.path().join("graph.sqlite"), Uuid::nil()).unwrap();
    let scope = store.scope(WorkspaceInstanceKey::Primary);
    let run = store
        .begin_staged(&scope, &manifest(&metadata.metadata_digest), &versions)
        .unwrap();
    for facts in &extracted.facts {
        store.stage_file(&run, facts).unwrap();
    }
    let first = store.publish_staged(&run, PublishFault::None).unwrap();
    let completeness = store.current_completeness(scope.workspace_id()).unwrap();
    assert_eq!(
        (
            completeness.parsed_files,
            completeness.degraded_files,
            completeness.total_files,
        ),
        (parsed, paths.len() - parsed, paths.len())
    );
    let matches = store
        .search_name(scope.workspace_id(), "CodegraphStore", 16)
        .unwrap();
    assert!(
        matches
            .nodes
            .iter()
            .any(|node| node.kind == NodeKind::Struct
                && node.span.path == "crates/rsi-codegraph/src/lib.rs")
    );
    let target = matches
        .nodes
        .iter()
        .find(|node| {
            node.kind == NodeKind::Struct && node.span.path == "crates/rsi-codegraph/src/lib.rs"
        })
        .unwrap();
    let explanation = store.explain(scope.workspace_id(), target.id).unwrap();
    assert_eq!(explanation.node.name, "CodegraphStore");
    let rg = Command::new("rg")
        .args([
            "-n",
            "struct CodegraphStore",
            "crates/rsi-codegraph/src/lib.rs",
        ])
        .current_dir(&corpus)
        .output()
        .unwrap();
    assert!(rg.status.success());
    let rg_hits = String::from_utf8_lossy(&rg.stdout).lines().count();
    eprintln!(
        "S1 rg baseline: hits={} elapsed={:?} output={}",
        rg_hits,
        started.elapsed(),
        String::from_utf8_lossy(&rg.stdout).trim(),
    );
    let clean = ExtractionCache::new()
        .update_with_cargo(sources, &metadata)
        .unwrap();
    assert_eq!(extracted.facts, clean.facts);
    assert_eq!(extracted.inventory.digest, clean.inventory.digest);
    let run = store
        .begin_staged(&scope, &manifest(&metadata.metadata_digest), &versions)
        .unwrap();
    for facts in &clean.facts {
        store.stage_file(&run, facts).unwrap();
    }
    let second = store.publish_staged(&run, PublishFault::None).unwrap();
    assert_eq!(first.snapshot_digest, second.snapshot_digest);
    assert_eq!(first.graph_digest, second.graph_digest);
    let golden = format!(
        "files={}\nsymlinks_excluded={}\nrust={}\nmarkdown={}\ntoml={}\nbytes={}\nparsed={}\nparse_errors={}\nunsupported={}\noversize={}\nnon_utf8={}\nnodes={}\nrelations={}\nunresolved={}\ncargo_dependencies={}\nexact_cargo_sites={}\nrg_hits={}\ninventory_digest={}\nsnapshot_digest={}\ngraph_digest={}",
        paths.len(),
        symlinks,
        counts[0],
        counts[1],
        counts[2],
        source_bytes,
        parsed,
        diagnostics[1],
        diagnostics[2],
        diagnostics[3],
        diagnostics[4],
        nodes,
        relations,
        unresolved,
        metadata.dependencies.len(),
        metadata
            .dependencies
            .iter()
            .filter(|dependency| dependency.manifest_span.is_some())
            .count(),
        rg_hits,
        extracted.inventory.digest,
        first.snapshot_digest,
        first.graph_digest,
    );
    eprintln!("S1 corpus elapsed: {:?}\n{golden}", started.elapsed());
    assert_eq!(golden, include_str!("golden/s1-corpus.txt").trim());
}
