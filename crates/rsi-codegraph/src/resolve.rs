//! Exact, unique-proof reference resolution. Unsupported or ambiguous sites
//! remain explicit unresolved facts; name similarity never becomes an edge.

use std::collections::HashMap;

use crate::{
    EvidenceFact, FactKey, FactProvenance, NodeFact, NodeKind, RelationFact, RelationKind,
    UnresolvedReferenceFact, UnresolvedReferenceKind, staged::PerFileFacts,
};

/// Resolve references with an explicit crate-root path and exactly one matching
/// Rust declaration. Unqualified calls and method dispatch require compiler
/// scope/type facts and remain unresolved.
pub fn resolve_unique(files: &mut [PerFileFacts]) {
    let mut by_semantic_path: HashMap<String, Vec<FactKey>> = HashMap::new();
    let mut owner_crates: HashMap<FactKey, String> = HashMap::new();
    let mut findings: HashMap<String, Vec<FactKey>> = HashMap::new();
    for file in files.iter() {
        if !file.diagnostic.eq(&crate::FileDiagnostic::Parsed) {
            continue;
        }
        for node in &file.nodes {
            if let Some((crate_root, semantic)) = rust_semantic_path(node) {
                owner_crates.insert(node.key.clone(), crate_root.clone());
                by_semantic_path
                    .entry(format!("{crate_root}\0{semantic}"))
                    .or_default()
                    .push(node.key.clone());
            } else if node.kind == NodeKind::ResearchFinding
                && let Some(id) = node.name.get(..5).filter(|id| {
                    id.starts_with("F-") && id[2..].bytes().all(|byte| byte.is_ascii_digit())
                })
            {
                findings
                    .entry(id.to_owned())
                    .or_default()
                    .push(node.key.clone());
            }
        }
    }
    for file in files.iter_mut() {
        if file.diagnostic != crate::FileDiagnostic::Parsed {
            continue;
        }
        let mut remaining = Vec::with_capacity(file.unresolved_references.len());
        for reference in std::mem::take(&mut file.unresolved_references) {
            let target = match reference.kind {
                UnresolvedReferenceKind::Call
                | UnresolvedReferenceKind::Import
                | UnresolvedReferenceKind::Type
                | UnresolvedReferenceKind::ImplTrait => {
                    owner_crates.get(&reference.owner).and_then(|crate_root| {
                        let raw = reference.raw_target.trim();
                        if !raw.starts_with("crate::")
                            || raw.contains(['{', '}', '*', '(', ')', ' '])
                        {
                            return None;
                        }
                        let key = format!("{crate_root}\0{raw}");
                        unique(by_semantic_path.get(&key))
                    })
                }
                UnresolvedReferenceKind::Finding => unique(findings.get(&reference.raw_target)),
                _ => None,
            };
            if let Some(target) = target
                && target != reference.owner
            {
                file.relations.push(resolved_relation(&reference, &target));
                continue;
            }
            remaining.push(reference);
        }
        file.unresolved_references = remaining;
    }
}

fn unique(candidates: Option<&Vec<FactKey>>) -> Option<FactKey> {
    let candidates = candidates?;
    (candidates.len() == 1).then(|| candidates[0].clone())
}

fn rust_semantic_path(node: &NodeFact) -> Option<(String, String)> {
    if node.identity.language != "rust" {
        return None;
    }
    let path = &node.span.path;
    let (crate_root, source) = path.rsplit_once("/src/").map_or_else(
        || {
            path.strip_prefix("src/")
                .map(|source| (String::new(), source))
        },
        |(root, source)| Some((root.to_owned(), source)),
    )?;
    let module = source.strip_suffix(".rs")?;
    let module = module.strip_suffix("/mod").unwrap_or(module);
    let module = if matches!(module, "lib" | "main") {
        ""
    } else {
        module
    };
    let suffix = node.identity.qualified_name.strip_prefix(path)?;
    let suffix = suffix.strip_prefix("::").unwrap_or(suffix);
    let mut semantic = String::from("crate");
    if !module.is_empty() {
        semantic.push_str("::");
        semantic.push_str(&module.replace('/', "::"));
    }
    if !suffix.is_empty() {
        semantic.push_str("::");
        semantic.push_str(suffix);
    }
    Some((crate_root, semantic))
}

fn resolved_relation(reference: &UnresolvedReferenceFact, target: &FactKey) -> RelationFact {
    let kind = match reference.kind {
        UnresolvedReferenceKind::Call => RelationKind::Calls,
        UnresolvedReferenceKind::Type => RelationKind::UsesType,
        UnresolvedReferenceKind::ImplTrait => RelationKind::Implements,
        UnresolvedReferenceKind::Import => RelationKind::Imports,
        UnresolvedReferenceKind::Finding => RelationKind::ReferencesFinding,
        _ => unreachable!("only supported reference kinds are resolved"),
    };
    let digest = blake3::hash(format!("{}\0{}\0{kind:?}", reference.key.0, target.0).as_bytes())
        .to_hex()
        .to_string();
    RelationFact {
        key: FactKey(format!("v1:r:{digest}")),
        owner_file: reference.span.path.clone(),
        site_anchor: format!("v1:{digest}"),
        kind,
        source: reference.owner.clone(),
        target: target.clone(),
        provenance: FactProvenance::Extracted,
        evidence: vec![EvidenceFact {
            label: "unique source reference".into(),
            span: reference.span.clone(),
        }],
    }
}
