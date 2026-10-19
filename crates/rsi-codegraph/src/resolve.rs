//! Reference resolution. Rust references go through the module-tree resolver
//! in `rust_resolve`: an unambiguous proof becomes an `Extracted` edge, a
//! bounded set of candidates becomes `Inferred` edges, and anything else stays
//! an explicit unresolved fact. Name similarity alone never becomes an
//! `Extracted` edge.

use std::collections::HashMap;

use crate::{
    EvidenceFact, FactKey, FactProvenance, NodeKind, RelationFact, RelationKind,
    UnresolvedReferenceFact, UnresolvedReferenceKind, cargo_metadata::CargoWorkspace, rust_resolve,
    staged::PerFileFacts,
};

/// Resolve references across the corpus without Cargo metadata: crate roots
/// come from Cargo's default target layout and every workspace library is
/// visible by name.
pub fn resolve_unique(files: &mut [PerFileFacts]) {
    resolve_workspace(files, None);
}

/// Resolve references across the corpus. With Cargo metadata, crate roots,
/// target kinds and dependency names are exact.
pub fn resolve_workspace(files: &mut [PerFileFacts], cargo: Option<&CargoWorkspace>) {
    let outcomes = rust_resolve::resolve(files, cargo);
    for (file, outcome) in files.iter_mut().zip(outcomes) {
        if !outcome.relations.is_empty() {
            file.relations.extend(outcome.relations);
        }
        if !outcome.dropped.is_empty() {
            let mut position = 0usize;
            file.unresolved_references.retain(|_| {
                let keep = !outcome.dropped.contains(&position);
                position += 1;
                keep
            });
        }
    }
    resolve_findings(files);
}

fn resolve_findings(files: &mut [PerFileFacts]) {
    let mut findings: HashMap<String, Vec<FactKey>> = HashMap::new();
    for file in files.iter() {
        if file.diagnostic != crate::FileDiagnostic::Parsed {
            continue;
        }
        for node in &file.nodes {
            if node.kind == NodeKind::ResearchFinding
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
            if reference.kind == UnresolvedReferenceKind::Finding
                && let Some(target) = unique(findings.get(&reference.raw_target))
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

fn resolved_relation(reference: &UnresolvedReferenceFact, target: &FactKey) -> RelationFact {
    let kind = RelationKind::ReferencesFinding;
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
