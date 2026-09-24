//! Bounded, source-owned structural extraction. Cross-file references remain
//! unresolved until a separate resolver can prove one target.

mod cargo;
mod markdown;
mod rust;

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::Node;

use crate::{
    EvidenceFact, FactKey, FactProvenance, NodeFact, NodeIdentity, NodeKind, RelationFact,
    RelationKind, SourceFile, SourceSpan, UnresolvedReferenceFact, UnresolvedReferenceKind,
    staged::{FileDiagnostic, PARSER_MAX_FILE_BYTES, PerFileFacts},
};

/// Extract one exact source version. Parse errors retain proven Rust declarations
/// from clean syntax subtrees; other degraded files contribute no facts.
#[must_use]
pub fn extract_file(file: SourceFile) -> PerFileFacts {
    let mut output = PerFileFacts {
        file,
        nodes: Vec::new(),
        relations: Vec::new(),
        unresolved_references: Vec::new(),
        diagnostic: FileDiagnostic::Parsed,
    };
    let bytes = &output.file.bytes;
    if bytes.len() > PARSER_MAX_FILE_BYTES {
        output.diagnostic = FileDiagnostic::Oversize { bytes: bytes.len() };
        return output;
    }
    if std::str::from_utf8(bytes).is_err() {
        output.diagnostic = FileDiagnostic::NonUtf8;
        return output;
    }
    let path = output.file.relative_path.as_str();
    let extension = Path::new(path).extension().and_then(|value| value.to_str());
    let is_rust = extension.is_some_and(|value| value.eq_ignore_ascii_case("rs"));
    let is_markdown = extension.is_some_and(|value| {
        value.eq_ignore_ascii_case("md") || value.eq_ignore_ascii_case("markdown")
    });
    let is_toml = extension.is_some_and(|value| value.eq_ignore_ascii_case("toml"));
    let language = if is_rust {
        Some(tree_sitter_rust::LANGUAGE)
    } else if is_markdown {
        Some(tree_sitter_md::LANGUAGE)
    } else if is_toml {
        Some(tree_sitter_toml_ng::LANGUAGE)
    } else {
        None
    };
    let Some(language) = language else {
        output.diagnostic = FileDiagnostic::Unsupported {
            reason: "no supported extractor for this path".into(),
        };
        return output;
    };
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter::Language::from(language))
        .is_err()
    {
        output.diagnostic = FileDiagnostic::Unsupported {
            reason: "tree-sitter grammar is incompatible".into(),
        };
        return output;
    }
    let Some(tree) = parser.parse(bytes, None) else {
        output.diagnostic = FileDiagnostic::Unsupported {
            reason: "tree-sitter parser returned no tree".into(),
        };
        return output;
    };
    let root = tree.root_node();
    let errors = error_count(root);
    if errors > 0 {
        output.diagnostic = FileDiagnostic::ParseErrors { count: errors };
    }
    let mut builder = Builder::new(&output.file);
    if is_rust {
        rust::extract(root, &mut builder);
    } else if errors > 0 {
        return output;
    } else if is_markdown {
        markdown::extract(root, &mut builder);
    } else {
        cargo::extract(root, &mut builder);
    }
    if errors > 0 {
        builder.nodes.retain(|node| node.kind != NodeKind::File);
        builder.relations.clear();
        builder.unresolved.clear();
    }
    output.nodes = builder.nodes;
    output.relations = builder.relations;
    output.unresolved_references = builder.unresolved;
    output
}

fn error_count(node: Node<'_>) -> usize {
    let mut count = usize::from(node.is_error() || node.is_missing());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count += error_count(child);
    }
    count
}

struct Builder<'a> {
    file: &'a SourceFile,
    nodes: Vec<NodeFact>,
    relations: Vec<RelationFact>,
    unresolved: Vec<UnresolvedReferenceFact>,
    node_counts: HashMap<(NodeKind, String), usize>,
    site_counts: HashMap<String, usize>,
    line_starts: Vec<usize>,
}

impl<'a> Builder<'a> {
    fn new(file: &'a SourceFile) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            file.bytes
                .iter()
                .enumerate()
                .filter_map(|(at, byte)| (*byte == b'\n').then_some(at + 1)),
        );
        Self {
            file,
            nodes: Vec::new(),
            relations: Vec::new(),
            unresolved: Vec::new(),
            node_counts: HashMap::new(),
            site_counts: HashMap::new(),
            line_starts,
        }
    }

    fn text(&self, node: Node<'_>) -> &'a str {
        std::str::from_utf8(&self.file.bytes[node.byte_range()]).unwrap_or("")
    }

    fn span(&self, start: usize, end: usize) -> SourceSpan {
        let (start_line, start_column) = self.line_column(start);
        let (end_line, end_column) = self.line_column(end);
        SourceSpan {
            path: self.file.relative_path.clone(),
            start_byte: start,
            end_byte: end,
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    fn line_column(&self, offset: usize) -> (usize, usize) {
        let line_index = self.line_starts.partition_point(|start| *start <= offset) - 1;
        (line_index + 1, offset - self.line_starts[line_index] + 1)
    }

    fn node(
        &mut self,
        kind: NodeKind,
        language: &str,
        qualified: &str,
        name: &str,
        start: usize,
        end: usize,
    ) -> FactKey {
        let ordinal = self
            .node_counts
            .entry((kind, qualified.to_owned()))
            .or_default();
        let identity = format!(
            "{}\0{}\0{}\0{}",
            self.file.relative_path,
            kind.as_str(),
            qualified,
            *ordinal
        );
        *ordinal += 1;
        let digest = blake3::hash(identity.as_bytes()).to_hex().to_string();
        let key = FactKey(format!("v1:n:{digest}"));
        self.nodes.push(NodeFact {
            key: key.clone(),
            identity: NodeIdentity {
                language: language.into(),
                qualified_name: bounded(qualified),
                disambiguator: digest,
            },
            kind,
            name: bounded(name),
            span: self.span(start, end),
            provenance: FactProvenance::Extracted,
            evidence: vec![EvidenceFact {
                label: "source declaration".into(),
                span: self.span(start, end.min(start + crate::MAX_EVIDENCE_SPAN_BYTES)),
            }],
        });
        key
    }

    fn relation(
        &mut self,
        kind: RelationKind,
        source: &FactKey,
        target: &FactKey,
        label: &str,
        start: usize,
        end: usize,
    ) {
        let site = format!("{}\0{}\0{}\0{}", source.0, target.0, kind.as_str(), label);
        let ordinal = self.site_counts.entry(site.clone()).or_default();
        let anchor_digest = blake3::hash(format!("{site}\0{ordinal}").as_bytes())
            .to_hex()
            .to_string();
        *ordinal += 1;
        self.relations.push(RelationFact {
            key: FactKey(format!("v1:r:{anchor_digest}")),
            owner_file: self.file.relative_path.clone(),
            site_anchor: format!("v1:{anchor_digest}"),
            kind,
            source: source.clone(),
            target: target.clone(),
            provenance: FactProvenance::Extracted,
            evidence: vec![EvidenceFact {
                label: bounded(label),
                span: self.span(start, end),
            }],
        });
    }

    fn unresolved(
        &mut self,
        owner: &FactKey,
        kind: UnresolvedReferenceKind,
        raw: &str,
        start: usize,
        end: usize,
    ) {
        let site = format!("{}\0{kind:?}\0{raw}", owner.0);
        let ordinal = self.site_counts.entry(site.clone()).or_default();
        let digest = blake3::hash(format!("{site}\0{ordinal}").as_bytes())
            .to_hex()
            .to_string();
        *ordinal += 1;
        self.unresolved.push(UnresolvedReferenceFact {
            key: FactKey(format!("v1:u:{digest}")),
            owner: owner.clone(),
            kind,
            raw_target: bounded(raw),
            span: self.span(start, end),
            provenance: FactProvenance::Extracted,
        });
    }
}

fn bounded(value: &str) -> String {
    if value.len() <= crate::MAX_NAME_BYTES {
        return value.to_owned();
    }
    format!("blake3:{}", blake3::hash(value.as_bytes()).to_hex())
}
