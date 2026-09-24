//! Bounded Cargo authority for workspace packages, targets, and resolved direct
//! dependencies. A missing unique manifest site stays unprojected.

use std::{
    collections::{HashMap, HashSet},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::{
    CodegraphError, EvidenceFact, FactKey, FactProvenance, NodeFact, NodeIdentity, NodeKind,
    RelationFact, RelationKind, Result, SourceSpan, UnresolvedReferenceKind, staged::PerFileFacts,
};

const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 128 * 1024;
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoWorkspace {
    pub packages: Vec<CargoPackage>,
    pub dependencies: Vec<CargoDependency>,
    pub metadata_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoPackage {
    pub id: String,
    pub name: String,
    pub manifest_path: String,
    pub targets: Vec<CargoTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoTarget {
    pub name: String,
    pub kinds: Vec<String>,
    pub source_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoDependency {
    pub owner_package_id: String,
    pub package_id: String,
    pub package_name: String,
    pub alias: String,
    pub kind: RelationKind,
    pub target_condition: Option<String>,
    pub manifest_span: Option<SourceSpan>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
    resolve: Option<MetadataResolve>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
    targets: Vec<MetadataTarget>,
}

#[derive(Deserialize)]
struct MetadataTarget {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
}

#[derive(Deserialize)]
struct MetadataResolve {
    nodes: Vec<MetadataResolveNode>,
}

#[derive(Deserialize)]
struct MetadataResolveNode {
    id: String,
    deps: Vec<MetadataDep>,
}

#[derive(Deserialize)]
struct MetadataDep {
    name: String,
    pkg: String,
    dep_kinds: Vec<MetadataDepKind>,
}

#[derive(Deserialize)]
struct MetadataDepKind {
    kind: Option<String>,
    target: Option<String>,
}

/// Run offline locked Cargo metadata with a hard wall and output cap. Only
/// workspace manifests and targets under the supplied root enter the result.
///
/// # Errors
/// Returns visible path, timeout, process, output-bound, or JSON errors.
pub fn read_workspace(root: &Path) -> Result<CargoWorkspace> {
    let root = root.canonicalize().map_err(io_error)?;
    if !root.join("Cargo.toml").is_file() {
        return Err(CodegraphError::InvalidInput(
            "Cargo.toml is missing from workspace root".into(),
        ));
    }
    let mut stdout = tempfile::tempfile().map_err(io_error)?;
    let mut stderr = tempfile::tempfile().map_err(io_error)?;
    let mut child = Command::new("cargo")
        .args(["metadata", "--offline", "--locked", "--format-version", "1"])
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().map_err(io_error)?))
        .stderr(Stdio::from(stderr.try_clone().map_err(io_error)?))
        .spawn()
        .map_err(io_error)?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(io_error)? {
            break status;
        }
        if start.elapsed() > METADATA_TIMEOUT {
            child.kill().map_err(io_error)?;
            child.wait().map_err(io_error)?;
            return Err(CodegraphError::InvalidInput(
                "cargo metadata exceeded 30-second deadline".into(),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = read_bounded(&mut stdout, MAX_METADATA_BYTES)?;
    if !status.success() {
        let error = read_bounded(&mut stderr, MAX_STDERR_BYTES)?;
        return Err(CodegraphError::InvalidInput(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&error)
        )));
    }
    let metadata: Metadata = serde_json::from_slice(&output).map_err(|error| {
        CodegraphError::InvalidInput(format!("invalid cargo metadata JSON: {error}"))
    })?;
    project_metadata(&root, &output, metadata)
}

#[allow(clippy::too_many_lines)] // Projection validates packages, targets, and resolved edges as one input.
fn project_metadata(root: &Path, output: &[u8], metadata: Metadata) -> Result<CargoWorkspace> {
    let workspace_members = metadata
        .workspace_members
        .into_iter()
        .collect::<HashSet<_>>();
    let package_names = metadata
        .packages
        .iter()
        .map(|package| (package.id.clone(), package.name.clone()))
        .collect::<HashMap<_, _>>();
    let workspace_ids = metadata
        .packages
        .iter()
        .filter(|package| workspace_members.contains(&package.id))
        .map(|package| {
            Ok((
                package.id.clone(),
                format!(
                    "workspace:{}:{}",
                    relative(root, &package.manifest_path)?,
                    package.name
                ),
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let mut packages = Vec::new();
    let mut manifests = HashMap::new();
    for package in metadata.packages {
        if !workspace_members.contains(&package.id) {
            continue;
        }
        let manifest_path = relative(root, &package.manifest_path)?;
        let manifest_bytes = std::fs::read(root.join(&manifest_path)).map_err(io_error)?;
        let targets = package
            .targets
            .into_iter()
            .map(|target| {
                Ok(CargoTarget {
                    name: target.name,
                    kinds: target.kind,
                    source_path: relative(root, &target.src_path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        manifests.insert(package.id.clone(), manifest_bytes);
        packages.push(CargoPackage {
            id: package.id,
            name: package.name,
            manifest_path,
            targets,
        });
    }
    packages.sort_by(|left, right| left.manifest_path.cmp(&right.manifest_path));
    let package_by_id = packages
        .iter()
        .map(|package| (package.id.as_str(), package))
        .collect::<HashMap<_, _>>();
    let mut dependencies = Vec::new();
    for node in metadata
        .resolve
        .ok_or_else(|| CodegraphError::InvalidInput("cargo metadata has no resolve graph".into()))?
        .nodes
    {
        let Some(owner) = package_by_id.get(node.id.as_str()) else {
            continue;
        };
        let bytes = &manifests[&node.id];
        for dep in node.deps {
            let package_name = package_names.get(&dep.pkg).ok_or_else(|| {
                CodegraphError::InvalidInput(format!("resolved Cargo package missing: {}", dep.pkg))
            })?;
            for dep_kind in dep.dep_kinds {
                let kind = match dep_kind.kind.as_deref() {
                    None | Some("normal") => RelationKind::DependsOn,
                    Some("dev") => RelationKind::DevDependsOn,
                    Some("build") => RelationKind::BuildDependsOn,
                    Some(other) => {
                        return Err(CodegraphError::InvalidInput(format!(
                            "unsupported Cargo dependency kind: {other}"
                        )));
                    }
                };
                dependencies.push(CargoDependency {
                    owner_package_id: node.id.clone(),
                    package_id: dep.pkg.clone(),
                    package_name: package_name.clone(),
                    alias: dep.name.clone(),
                    kind,
                    target_condition: dep_kind.target.clone(),
                    manifest_span: manifest_site(
                        &owner.manifest_path,
                        bytes,
                        &dep.name,
                        kind,
                        dep_kind.target.as_deref(),
                    ),
                });
            }
        }
    }
    dependencies.sort_by(|left, right| {
        (
            &left.owner_package_id,
            &left.alias,
            format!("{:?}", left.kind),
            &left.package_id,
        )
            .cmp(&(
                &right.owner_package_id,
                &right.alias,
                format!("{:?}", right.kind),
                &right.package_id,
            ))
    });
    for package in &mut packages {
        package.id = workspace_ids[&package.id].clone();
    }
    for dependency in &mut dependencies {
        dependency.owner_package_id = workspace_ids[&dependency.owner_package_id].clone();
        if let Some(id) = workspace_ids.get(&dependency.package_id) {
            dependency.package_id = id.clone();
        }
    }
    let canonical_bytes = canonical_metadata_bytes(root, output)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rsi-codegraph-cargo-metadata-v3");
    hasher.update(&canonical_bytes);
    if let Ok(lock) = std::fs::read(root.join("Cargo.lock")) {
        hasher.update(&lock);
    }
    Ok(CargoWorkspace {
        packages,
        dependencies,
        metadata_digest: hasher.finalize().to_hex().to_string(),
    })
}

fn canonical_metadata_bytes(root: &Path, output: &[u8]) -> Result<Vec<u8>> {
    let mut canonical: serde_json::Value = serde_json::from_slice(output).map_err(|error| {
        CodegraphError::InvalidInput(format!("invalid Cargo metadata JSON: {error}"))
    })?;
    let fields = canonical
        .as_object_mut()
        .ok_or_else(|| CodegraphError::InvalidInput("Cargo metadata must be an object".into()))?;
    // Cargo's build output paths depend on the caller's CARGO_TARGET_DIR and
    // do not change the workspace source or resolved dependencies.
    fields.remove("target_directory");
    fields.remove("build_directory");
    normalize_workspace_paths(&mut canonical, &root.to_string_lossy());
    serde_json::to_vec(&canonical).map_err(|error| {
        CodegraphError::InvalidInput(format!("cannot encode Cargo metadata: {error}"))
    })
}

fn normalize_workspace_paths(value: &mut serde_json::Value, root: &str) {
    match value {
        serde_json::Value::String(text) => {
            *text = text.replace(root, "$WORKSPACE");
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_workspace_paths(item, root);
            }
        }
        serde_json::Value::Object(fields) => {
            for item in fields.values_mut() {
                normalize_workspace_paths(item, root);
            }
        }
        _ => {}
    }
}

fn manifest_site(
    path: &str,
    bytes: &[u8],
    alias: &str,
    kind: RelationKind,
    target: Option<&str>,
) -> Option<SourceSpan> {
    let source = std::str::from_utf8(bytes).ok()?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter::Language::from(tree_sitter_toml_ng::LANGUAGE))
        .ok()?;
    let tree = parser.parse(bytes, None)?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut matches = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for table in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "table")
    {
        let header = table.named_child(0)?;
        let table_name = source.get(header.byte_range())?;
        if let Some((section, _)) = table_name.rsplit_once('.')
            && let Some((declared_alias, start, end)) = syntax_key_site(header, source, false)
            && cargo_ident_eq(&declared_alias, alias)
            && dependency_role(section) == Some(kind)
            && target.is_none_or(|condition| table_name.contains(condition))
        {
            matches.push(source_span(path, bytes, start, end));
        }
        if dependency_role(table_name) != Some(kind)
            || !target.is_none_or(|condition| table_name.contains(condition))
        {
            continue;
        }
        let mut pairs = table.walk();
        for pair in table
            .named_children(&mut pairs)
            .filter(|node| node.kind() == "pair")
        {
            let Some(key) = pair.named_child(0) else {
                continue;
            };
            if let Some((declared_alias, start, end)) = syntax_key_site(key, source, true)
                && cargo_ident_eq(&declared_alias, alias)
            {
                matches.push(source_span(path, bytes, start, end));
            }
        }
    }
    (matches.len() == 1).then(|| matches.remove(0))
}

fn syntax_key_site(
    mut key: tree_sitter::Node<'_>,
    source: &str,
    first: bool,
) -> Option<(String, usize, usize)> {
    while key.kind() == "dotted_key" {
        let index = if first {
            0
        } else {
            key.named_child_count().checked_sub(1)?
        };
        key = key.named_child(index)?;
    }
    let raw = source.get(key.byte_range())?;
    let alias = raw.trim_matches(['"', '\'']);
    if alias.is_empty() {
        return None;
    }
    let offset = usize::from(alias.len() != raw.len());
    let start = key.start_byte() + offset;
    Some((alias.to_owned(), start, start + alias.len()))
}

fn source_span(path: &str, bytes: &[u8], start: usize, end: usize) -> SourceSpan {
    let (line, column) = line_column(bytes, start);
    SourceSpan {
        path: path.into(),
        start_byte: start,
        end_byte: end,
        start_line: line,
        start_column: column,
        end_line: line,
        end_column: column + end - start,
    }
}

fn cargo_ident_eq(source: &str, metadata: &str) -> bool {
    source.len() == metadata.len()
        && source.bytes().zip(metadata.bytes()).all(|(left, right)| {
            left == right || (left == b'-' && right == b'_') || (left == b'_' && right == b'-')
        })
}

fn dependency_role(table: &str) -> Option<RelationKind> {
    if table.ends_with("dev-dependencies") {
        Some(RelationKind::DevDependsOn)
    } else if table.ends_with("build-dependencies") {
        Some(RelationKind::BuildDependsOn)
    } else if table.ends_with("dependencies") {
        Some(RelationKind::DependsOn)
    } else {
        None
    }
}

fn line_column(bytes: &[u8], offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for byte in &bytes[..offset] {
        if *byte == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    let path = path.canonicalize().map_err(io_error)?;
    let relative = path.strip_prefix(root).map_err(|_| {
        CodegraphError::InvalidInput(format!(
            "Cargo path escapes workspace root: {}",
            path.display()
        ))
    })?;
    let relative = relative
        .to_str()
        .ok_or_else(|| CodegraphError::InvalidInput("non-UTF8 Cargo path".into()))?;
    crate::validate_path(relative)?;
    Ok(relative.into())
}

fn read_bounded(file: &mut std::fs::File, maximum: usize) -> Result<Vec<u8>> {
    let length = usize::try_from(file.seek(SeekFrom::End(0)).map_err(io_error)?).map_err(|_| {
        CodegraphError::LimitExceeded {
            requested: usize::MAX,
            maximum,
        }
    })?;
    if length > maximum {
        return Err(CodegraphError::LimitExceeded {
            requested: length,
            maximum,
        });
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::with_capacity(length);
    file.read_to_end(&mut bytes).map_err(io_error)?;
    Ok(bytes)
}

#[allow(clippy::needless_pass_by_value)] // map_err passes its owned error to this adapter.
fn io_error(error: std::io::Error) -> CodegraphError {
    CodegraphError::InvalidInput(format!("Cargo metadata I/O: {error}"))
}

/// Add dependency edges only when Cargo resolved a package and one exact
/// manifest key is available. This consumes the matching unresolved site.
#[allow(clippy::too_many_lines)] // One package pass keeps target and dependency keys coordinated.
pub fn apply_workspace(facts: &mut [PerFileFacts], metadata: &CargoWorkspace) {
    let workspace_nodes = metadata
        .packages
        .iter()
        .filter_map(|package| {
            facts
                .iter()
                .find(|file| file.file.relative_path == package.manifest_path)
                .and_then(|file| {
                    file.nodes
                        .iter()
                        .find(|node| node.kind == NodeKind::Crate && node.name == package.name)
                })
                .map(|node| (package.id.as_str(), node.key.clone()))
        })
        .collect::<HashMap<_, _>>();
    for package in &metadata.packages {
        let Some(file) = facts
            .iter_mut()
            .find(|file| file.file.relative_path == package.manifest_path)
        else {
            continue;
        };
        let Some(owner) = workspace_nodes.get(package.id.as_str()) else {
            continue;
        };
        let Some(package_span) = file
            .nodes
            .iter()
            .find(|node| node.key == *owner)
            .map(|node| node.span.clone())
        else {
            continue;
        };
        for target in &package.targets {
            let existing = file
                .nodes
                .iter()
                .find(|node| node.kind == NodeKind::CargoTarget && node.name == target.name)
                .map(|node| node.key.clone());
            let target_key = existing.unwrap_or_else(|| {
                let identity = format!(
                    "{}\0{}\0{}",
                    package.id,
                    target.kinds.join(","),
                    target.name
                );
                let digest = blake3::hash(identity.as_bytes()).to_hex().to_string();
                let key = FactKey(format!("v1:cargo-target:{digest}"));
                file.nodes.push(NodeFact {
                    key: key.clone(),
                    identity: NodeIdentity {
                        language: "cargo".into(),
                        qualified_name: format!("{}::{}", package.name, target.name),
                        disambiguator: digest,
                    },
                    kind: NodeKind::CargoTarget,
                    name: target.name.clone(),
                    span: package_span.clone(),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![EvidenceFact {
                        label: "cargo metadata target; package manifest anchor".into(),
                        span: package_span.clone(),
                    }],
                });
                key
            });
            if !file.relations.iter().any(|relation| {
                relation.kind == RelationKind::Contains
                    && relation.source == *owner
                    && relation.target == target_key
            }) {
                let digest =
                    blake3::hash(format!("{}\0{}\0target", owner.0, target_key.0).as_bytes())
                        .to_hex()
                        .to_string();
                file.relations.push(RelationFact {
                    key: FactKey(format!("v1:r:{digest}")),
                    owner_file: package.manifest_path.clone(),
                    site_anchor: format!("v1:{digest}"),
                    kind: RelationKind::Contains,
                    source: owner.clone(),
                    target: target_key,
                    provenance: FactProvenance::Extracted,
                    evidence: vec![EvidenceFact {
                        label: "cargo metadata target; package manifest anchor".into(),
                        span: package_span.clone(),
                    }],
                });
            }
        }
        let mut external: HashMap<String, FactKey> = HashMap::new();
        for dependency in metadata
            .dependencies
            .iter()
            .filter(|dependency| dependency.owner_package_id == package.id)
        {
            let Some(span) = &dependency.manifest_span else {
                continue;
            };
            let target = if let Some(key) = workspace_nodes.get(dependency.package_id.as_str()) {
                key.clone()
            } else if let Some(key) = external.get(&dependency.package_id) {
                key.clone()
            } else {
                let digest = blake3::hash(
                    format!("{}\0{}", package.manifest_path, dependency.package_id).as_bytes(),
                )
                .to_hex()
                .to_string();
                let key = FactKey(format!("v1:cargo-pkg:{digest}"));
                file.nodes.push(NodeFact {
                    key: key.clone(),
                    identity: NodeIdentity {
                        language: "cargo".into(),
                        qualified_name: format!("package:{}", dependency.package_name),
                        disambiguator: digest,
                    },
                    kind: NodeKind::Crate,
                    name: dependency.package_name.clone(),
                    span: span.clone(),
                    provenance: FactProvenance::Extracted,
                    evidence: vec![EvidenceFact {
                        label: "cargo resolved package".into(),
                        span: span.clone(),
                    }],
                });
                external.insert(dependency.package_id.clone(), key.clone());
                key
            };
            let site = blake3::hash(
                format!(
                    "{}\0{}\0{:?}\0{}",
                    owner.0, target.0, dependency.kind, span.start_byte
                )
                .as_bytes(),
            )
            .to_hex()
            .to_string();
            file.relations.push(RelationFact {
                key: FactKey(format!("v1:r:{site}")),
                owner_file: package.manifest_path.clone(),
                site_anchor: format!("v1:{site}"),
                kind: dependency.kind,
                source: owner.clone(),
                target,
                provenance: FactProvenance::Extracted,
                evidence: vec![EvidenceFact {
                    label: "cargo metadata and manifest key".into(),
                    span: span.clone(),
                }],
            });
            file.unresolved_references.retain(|reference| {
                !(reference.kind == UnresolvedReferenceKind::Other
                    && cargo_ident_eq(&reference.raw_target, &dependency.alias)
                    && reference.span.start_byte == span.start_byte)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)] // Temporary Cargo fixture and subprocess output are test prerequisites.
    fn metadata_digest_is_independent_of_cargo_build_output_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        assert!(
            Command::new("cargo")
                .args(["generate-lockfile", "--offline"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );

        let read_at = |target: &Path| {
            let output = Command::new("cargo")
                .args(["metadata", "--offline", "--locked", "--format-version", "1"])
                .env("CARGO_TARGET_DIR", target)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(output.status.success());
            let raw: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let parsed: Metadata = serde_json::from_slice(&output.stdout).unwrap();
            let projected = project_metadata(root, &output.stdout, parsed).unwrap();
            (raw, projected)
        };
        let (first_raw, first) = read_at(&root.join("build-a"));
        let (second_raw, second) = read_at(&root.join("build-b"));
        assert_ne!(
            first_raw["target_directory"],
            second_raw["target_directory"]
        );
        assert_ne!(first_raw["build_directory"], second_raw["build_directory"]);
        assert_eq!(first, second);
    }
}
