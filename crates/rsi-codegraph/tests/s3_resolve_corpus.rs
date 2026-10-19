#![allow(clippy::expect_used, clippy::unwrap_used)] // Exact-source fixtures.

//! Resolution rate and precision on the frozen corpus (#2244): copies of
//! `impact.rs`, `resolve.rs` and `bin/rsi-test-impact.rs` plus a minimal crate
//! root and manifest. The corpus is deliberately not the live crate sources, so
//! the numbers stay comparable when those files change.

use std::collections::{BTreeSet, HashMap, HashSet};

use rsi_codegraph::{
    FactProvenance, NodeKind, PerFileFacts, RelationKind, SourceFile, UnresolvedReferenceKind,
    extract::extract_file, invalidate::ExtractionCache,
};

const CARGO: &str = include_str!("fixtures/resolve-corpus/Cargo.toml.txt");
const LIB: &str = include_str!("fixtures/resolve-corpus/lib.rs.txt");
const IMPACT: &str = include_str!("fixtures/resolve-corpus/impact.rs.txt");
const RESOLVE: &str = include_str!("fixtures/resolve-corpus/resolve.rs.txt");
const BIN: &str = include_str!("fixtures/resolve-corpus/rsi-test-impact.rs.txt");

fn corpus() -> Vec<SourceFile> {
    [
        ("Cargo.toml", CARGO),
        ("src/lib.rs", LIB),
        ("src/impact.rs", IMPACT),
        ("src/resolve.rs", RESOLVE),
        ("src/bin/rsi-test-impact.rs", BIN),
    ]
    .into_iter()
    .map(|(path, text)| SourceFile {
        relative_path: path.into(),
        bytes: text.as_bytes().to_vec(),
    })
    .collect()
}

type Site = (String, usize, usize);

fn site(span: &rsi_codegraph::SourceSpan) -> Site {
    (span.path.clone(), span.start_byte, span.end_byte)
}

/// The last name a call site mentions: `a.b().c` and `x::y::c::<T>` both give `c`.
fn callee_name(raw: &str) -> &str {
    let raw = raw.split("::<").next().unwrap_or(raw);
    let raw = raw.rsplit('.').next().unwrap_or(raw);
    raw.rsplit("::").next().unwrap_or(raw)
}

struct Measured {
    sites: usize,
    extracted: usize,
    inferred: usize,
    /// Sites naming a function or method that exists in the corpus.
    nameable: usize,
    nameable_extracted: usize,
    nameable_inferred: usize,
    legacy_crate_prefixed: usize,
    extracted_edges: Vec<String>,
    inferred_edges: Vec<String>,
    /// Sites naming a corpus function that stayed unresolved.
    missed: Vec<String>,
}

fn measure() -> Measured {
    let raw = corpus().into_iter().map(extract_file).collect::<Vec<_>>();
    let calls = raw
        .iter()
        .flat_map(|file| &file.unresolved_references)
        .filter(|reference| reference.kind == UnresolvedReferenceKind::Call)
        .collect::<Vec<_>>();
    let resolved = ExtractionCache::new().update(corpus()).unwrap().facts;
    let names: HashMap<_, _> = resolved
        .iter()
        .flat_map(|file| &file.nodes)
        .map(|node| {
            (
                node.key.clone(),
                format!("{}", node.identity.qualified_name),
            )
        })
        .collect();
    let defined: BTreeSet<&str> = resolved
        .iter()
        .flat_map(|file| &file.nodes)
        .filter(|node| matches!(node.kind, NodeKind::Function | NodeKind::Method))
        .map(|node| node.name.as_str())
        .collect();
    let mut extracted_sites = HashSet::new();
    let mut inferred_sites = HashSet::new();
    let mut extracted_edges = Vec::new();
    let mut inferred_edges = Vec::new();
    for relation in resolved.iter().flat_map(|file| &file.relations) {
        if relation.kind != RelationKind::Calls {
            continue;
        }
        let span = &relation.evidence[0].span;
        let line = format!(
            "{}:{} {} -> {}",
            span.path, span.start_line, names[&relation.source], names[&relation.target]
        );
        match relation.provenance {
            FactProvenance::Extracted => {
                extracted_sites.insert(site(span));
                extracted_edges.push(line);
            }
            FactProvenance::Inferred => {
                inferred_sites.insert(site(span));
                inferred_edges.push(line);
            }
            FactProvenance::Ambiguous => {}
        }
    }
    extracted_edges.sort();
    inferred_edges.sort();
    let mut measured = Measured {
        sites: calls.len(),
        extracted: 0,
        inferred: 0,
        nameable: 0,
        nameable_extracted: 0,
        nameable_inferred: 0,
        legacy_crate_prefixed: 0,
        extracted_edges,
        inferred_edges,
        missed: Vec::new(),
    };
    for call in calls {
        let key = site(&call.span);
        let is_extracted = extracted_sites.contains(&key);
        let is_inferred = !is_extracted && inferred_sites.contains(&key);
        measured.extracted += usize::from(is_extracted);
        measured.inferred += usize::from(is_inferred);
        if call.raw_target.starts_with("crate::")
            && !call.raw_target.contains(['{', '}', '*', '(', ')', ' '])
        {
            measured.legacy_crate_prefixed += 1;
        }
        if defined.contains(callee_name(&call.raw_target)) {
            measured.nameable += 1;
            measured.nameable_extracted += usize::from(is_extracted);
            measured.nameable_inferred += usize::from(is_inferred);
            if !is_extracted && !is_inferred {
                measured.missed.push(format!(
                    "{}:{} {}",
                    call.span.path, call.span.start_line, call.raw_target
                ));
            }
        }
    }
    measured
}

#[allow(clippy::cast_precision_loss)]
fn rate(part: usize, whole: usize) -> f64 {
    part as f64 / whole.max(1) as f64
}

fn report(measured: &Measured) -> String {
    format!(
        "call sites {} | extracted {} ({:.3}) | inferred {} | (extracted+inferred)/sites {:.3}\n\
         nameable sites {} | extracted {} | inferred {} | (extracted+inferred)/nameable {:.3} | extracted/nameable {:.3}\n\
         legacy `crate::`-prefixed call sites (all the old resolver could place): {}\n\
         EXTRACTED EDGES ({}):\n{}\nINFERRED EDGES ({}):\n{}\nMISSED NAMEABLE SITES ({}):\n{}",
        measured.sites,
        measured.extracted,
        rate(measured.extracted, measured.sites),
        measured.inferred,
        rate(measured.extracted + measured.inferred, measured.sites),
        measured.nameable,
        measured.nameable_extracted,
        measured.nameable_inferred,
        rate(
            measured.nameable_extracted + measured.nameable_inferred,
            measured.nameable
        ),
        rate(measured.nameable_extracted, measured.nameable),
        measured.legacy_crate_prefixed,
        measured.extracted_edges.len(),
        measured.extracted_edges.join("\n"),
        measured.inferred_edges.len(),
        measured.inferred_edges.join("\n"),
        measured.missed.len(),
        measured.missed.join("\n"),
    )
}

fn node_names(facts: &[PerFileFacts], kind: NodeKind) -> BTreeSet<String> {
    facts
        .iter()
        .flat_map(|file| &file.nodes)
        .filter(|node| node.kind == kind)
        .map(|node| node.identity.qualified_name.clone())
        .collect()
}

const GOLDEN: &str = include_str!("golden/resolve-corpus-calls.txt");

/// The hand-labelled edges, as (extracted, inferred) sorted lists.
fn golden() -> (Vec<String>, Vec<String>) {
    let mut extracted = Vec::new();
    let mut inferred = Vec::new();
    let mut section = &mut extracted;
    for line in GOLDEN.lines() {
        match line {
            "" => {}
            line if line.starts_with('#') => {}
            "[extracted]" => section = &mut extracted,
            "[inferred]" => section = &mut inferred,
            line => section.push(line.to_owned()),
        }
    }
    extracted.sort();
    inferred.sort();
    (extracted, inferred)
}

/// Before (#2244) the resolver placed only `crate::`-qualified paths: zero call
/// sites here. After, the numbers below are pinned so a regression is visible.
#[test]
fn resolution_rate_and_precision_on_the_frozen_corpus() {
    let measured = measure();
    let report = report(&measured);
    eprintln!("{report}");
    assert_eq!(measured.sites, 931, "{report}");
    assert_eq!(measured.legacy_crate_prefixed, 0, "{report}");
    let (extracted, inferred) = golden();
    // Every Extracted edge was checked by hand against its source line, so
    // equality with the labels is strict precision and recall on the labels.
    assert_eq!(measured.extracted_edges, extracted, "{report}");
    assert_eq!(measured.inferred_edges, inferred, "{report}");
    let nameable_rate = rate(
        measured.nameable_extracted + measured.nameable_inferred,
        measured.nameable,
    );
    assert!(nameable_rate >= 0.6, "{report}");
    assert!(rate(measured.extracted, measured.sites) >= 0.12, "{report}");
}

fn live_sources() -> Vec<SourceFile> {
    fn walk(root: &std::path::Path, directory: &std::path::Path, out: &mut Vec<SourceFile>) {
        let mut entries = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(SourceFile {
                    relative_path: path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    bytes: std::fs::read(&path).unwrap(),
                });
            }
        }
    }
    let root = std::path::Path::new(rsi_common::manifest_dir!());
    let mut sources = vec![SourceFile {
        relative_path: "Cargo.toml".into(),
        bytes: std::fs::read(root.join("Cargo.toml")).unwrap(),
    }];
    walk(root, &root.join("src"), &mut sources);
    sources
}

/// A precision invariant over the live crate: an `Extracted` call edge names a
/// callee whose last path segment is the callee's own name, unless the module
/// renamed it in a `use ... as` clause. This catches a resolver that links a
/// site to the wrong entity even where nobody has hand-labelled the edge.
#[test]
fn live_crate_extracted_calls_name_their_callee() {
    let sources = live_sources();
    let aliased = sources
        .iter()
        .flat_map(|source| {
            String::from_utf8_lossy(&source.bytes)
                .split(" as ")
                .skip(1)
                .map(|rest| {
                    rest.chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        })
        .collect::<HashSet<_>>();
    let facts = ExtractionCache::new().update(sources).unwrap().facts;
    let names: HashMap<_, _> = facts
        .iter()
        .flat_map(|file| &file.nodes)
        .map(|node| (node.key.clone(), node.name.clone()))
        .collect();
    let bytes: HashMap<_, _> = facts
        .iter()
        .map(|file| (file.file.relative_path.clone(), &file.file.bytes))
        .collect();
    let mut checked = 0usize;
    let mut wrong = Vec::new();
    for relation in facts.iter().flat_map(|file| &file.relations) {
        if relation.kind != RelationKind::Calls || relation.provenance != FactProvenance::Extracted
        {
            continue;
        }
        let span = &relation.evidence[0].span;
        let text = String::from_utf8_lossy(&bytes[&span.path][span.start_byte..span.end_byte])
            .into_owned();
        let callee = callee_name(&text).to_owned();
        if aliased.contains(&callee) {
            continue;
        }
        checked += 1;
        if names[&relation.target] != callee {
            wrong.push(format!(
                "{}:{} {text} -> {}",
                span.path, span.start_line, names[&relation.target]
            ));
        }
    }
    assert!(wrong.is_empty(), "{checked} checked; wrong: {wrong:#?}");
    assert!(
        checked >= 200,
        "only {checked} Extracted edges in the live crate"
    );
}

#[test]
fn corpus_has_the_expected_declarations() {
    let facts = ExtractionCache::new().update(corpus()).unwrap().facts;
    let functions = node_names(&facts, NodeKind::Function);
    assert!(functions.contains("src/impact.rs::select_tests_with_readers"));
    assert!(functions.contains("src/impact.rs::select_tests"));
    assert!(functions.contains("src/bin/rsi-test-impact.rs::run"));
}
