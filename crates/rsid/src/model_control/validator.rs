use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use rsi_common::model_control::PaidRisk;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Block, Expr, ExprAssign, ExprMacro, ExprMethodCall, ExprPath, FnArg, ImplItem, Item,
    ItemImpl, LitStr, Local, Pat, PatIdent, Signature, Type, UseTree,
};

use super::registry::{
    self, AllowedExclusionOperation, CapabilityConsumption, ExecutionBoundaryContract,
    ExecutionRoute, NonInvocationContract, PrimitiveKind,
};

const START_MARKER: &str = "## Registered Invocation Purposes";
const EXCLUSION_MARKER: &str = "## Explicit Non-Invocation Exclusions";

#[derive(Debug, Clone)]
pub enum RouteDisposition {
    Boundaries(Vec<String>),
    Excluded(String),
}

#[derive(Debug, Clone)]
pub struct PurposeDisposition {
    pub purpose: String,
    pub paid_risk: PaidRisk,
    pub route: RouteDisposition,
}

#[derive(Debug, Clone)]
pub struct ValidationInput {
    pub repo_root: PathBuf,
    pub source_root: PathBuf,
    pub markdown: Option<(PathBuf, String, String)>,
    pub purposes: Vec<PurposeDisposition>,
    pub boundaries: Vec<ExecutionBoundaryContract>,
    pub exclusions: Vec<NonInvocationContract>,
}

#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub inventory: Vec<String>,
}

impl ValidationInput {
    pub fn production(repo_root: PathBuf) -> Self {
        let purposes = registry::REGISTRY
            .iter()
            .map(|entry| {
                let route = match registry::execution_route(entry.purpose) {
                    ExecutionRoute::Boundaries(ids) => RouteDisposition::Boundaries(
                        ids.iter().map(|id| (*id).to_string()).collect(),
                    ),
                    ExecutionRoute::Excluded(id) => RouteDisposition::Excluded(id.to_string()),
                };
                PurposeDisposition {
                    purpose: entry.purpose.as_str().to_string(),
                    paid_risk: entry.paid_risk,
                    route,
                }
            })
            .collect();
        Self {
            source_root: repo_root.join("crates/rsid/src"),
            markdown: Some((
                repo_root.join("thoughts/shared/reference/model-invocation-registry.md"),
                registry::render_markdown_table(),
                registry::render_exclusion_markdown_table(),
            )),
            repo_root,
            purposes,
            boundaries: registry::EXECUTION_BOUNDARIES.to_vec(),
            exclusions: registry::NON_INVOCATIONS.to_vec(),
        }
    }
}

pub fn validate_repository(input: ValidationInput) -> Result<ValidationReport, String> {
    let mut violations = Vec::new();
    validate_markdown(&input, &mut violations);
    validate_routes(&input, &mut violations);

    let mut parsed_files = HashMap::new();
    for contract in &input.boundaries {
        validate_boundary(&input, contract, &mut parsed_files, &mut violations);
    }
    for contract in &input.exclusions {
        validate_exclusion(&input, contract, &mut parsed_files, &mut violations);
    }

    let discovered = discover_repository_sinks(&input, &mut parsed_files, &mut violations);
    validate_closed_join(&input, &discovered, &mut violations);

    if !violations.is_empty() {
        violations.sort();
        violations.dedup();
        return Err(format!(
            "model-control structural validation failed:\n{}",
            violations.join("\n")
        ));
    }

    let mut inventory = Vec::new();
    for purpose in &input.purposes {
        match &purpose.route {
            RouteDisposition::Boundaries(ids) => inventory.push(format!(
                "purpose={} risk={:?} boundaries={}",
                purpose.purpose,
                purpose.paid_risk,
                ids.join(",")
            )),
            RouteDisposition::Excluded(id) => inventory.push(format!(
                "purpose={} risk={:?} exclusion={id}",
                purpose.purpose, purpose.paid_risk
            )),
        }
    }
    for sink in discovered {
        inventory.push(format!(
            "sink={} item={} kind={:?} count={}",
            sink.path, sink.item, sink.kind, sink.count
        ));
    }
    inventory.sort();
    Ok(ValidationReport { inventory })
}

fn validate_markdown(input: &ValidationInput, violations: &mut Vec<String>) {
    let Some((path, expected_purposes, expected_exclusions)) = &input.markdown else {
        return;
    };
    match fs::read_to_string(path) {
        Ok(document) => {
            validate_markdown_table(
                input,
                path,
                &document,
                START_MARKER,
                expected_purposes,
                "registry",
                violations,
            );
            validate_markdown_table(
                input,
                path,
                &document,
                EXCLUSION_MARKER,
                expected_exclusions,
                "exclusion",
                violations,
            );
        }
        Err(error) => violations.push(format!(
            "{}: failed to read registry Markdown: {error}",
            relative(&input.repo_root, path)
        )),
    }
}

fn validate_markdown_table(
    input: &ValidationInput,
    path: &Path,
    document: &str,
    marker: &str,
    expected: &str,
    label: &str,
    violations: &mut Vec<String>,
) {
    match extract_table(document, marker) {
        Ok(actual) if normalize(&actual) == normalize(expected) => {}
        Ok(_) => violations.push(format!(
            "{}: {label} Markdown drift",
            relative(&input.repo_root, path)
        )),
        Err(error) => violations.push(format!("{}: {error}", relative(&input.repo_root, path))),
    }
}

fn extract_table(document: &str, marker: &str) -> Result<String, String> {
    let start = document
        .find(marker)
        .ok_or_else(|| format!("missing `{marker}` section"))?;
    let body = &document[start + marker.len()..];
    let section = body.find("\n## ").map_or(body, |end| &body[..end]);
    let table = section
        .find('|')
        .ok_or_else(|| "registry section has no Markdown table".to_string())?;
    Ok(section[table..].trim().to_string() + "\n")
}

fn normalize(value: &str) -> String {
    value.trim().replace("\r\n", "\n")
}

fn validate_routes(input: &ValidationInput, violations: &mut Vec<String>) {
    let boundary_ids = unique_ids(
        input.boundaries.iter().map(|contract| contract.id),
        "boundary",
        violations,
    );
    let exclusion_ids = unique_ids(
        input.exclusions.iter().map(|contract| contract.id),
        "exclusion",
        violations,
    );
    let mut purposes = HashSet::new();
    for purpose in &input.purposes {
        if !purposes.insert(purpose.purpose.as_str()) {
            violations.push(format!("duplicate purpose route `{}`", purpose.purpose));
        }
        match (&purpose.paid_risk, &purpose.route) {
            (PaidRisk::PaidCapable, RouteDisposition::Boundaries(ids)) if !ids.is_empty() => {
                for id in ids {
                    if !boundary_ids.contains(id.as_str()) {
                        violations.push(format!(
                            "purpose `{}` references missing boundary `{id}`",
                            purpose.purpose
                        ));
                    }
                }
            }
            (PaidRisk::PaidCapable, _) => violations.push(format!(
                "paid purpose `{}` has no execution boundary",
                purpose.purpose
            )),
            (PaidRisk::CatalogOnly | PaidRisk::NonInvocation, RouteDisposition::Excluded(id)) => {
                if !exclusion_ids.contains(id.as_str()) {
                    violations.push(format!(
                        "purpose `{}` references missing exclusion `{id}`",
                        purpose.purpose
                    ));
                }
            }
            (PaidRisk::CatalogOnly | PaidRisk::NonInvocation, _) => violations.push(format!(
                "non-invocation purpose `{}` maps to an execution boundary",
                purpose.purpose
            )),
            (PaidRisk::LocalOnly, RouteDisposition::Boundaries(ids)) if !ids.is_empty() => {
                for id in ids {
                    if !boundary_ids.contains(id.as_str()) {
                        violations.push(format!(
                            "local purpose `{}` references missing boundary `{id}`",
                            purpose.purpose
                        ));
                    }
                }
            }
            (PaidRisk::LocalOnly, RouteDisposition::Excluded(id)) => {
                if !exclusion_ids.contains(id.as_str()) {
                    violations.push(format!(
                        "local purpose `{}` references missing exclusion `{id}`",
                        purpose.purpose
                    ));
                }
            }
            (PaidRisk::LocalOnly, RouteDisposition::Boundaries(_)) => violations.push(format!(
                "local purpose `{}` has an empty execution route",
                purpose.purpose
            )),
        }
    }
}

fn unique_ids<'a>(
    ids: impl Iterator<Item = &'a str>,
    label: &str,
    violations: &mut Vec<String>,
) -> HashSet<&'a str> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            violations.push(format!("duplicate {label} id `{id}`"));
        }
    }
    seen
}

fn validate_boundary(
    input: &ValidationInput,
    contract: &ExecutionBoundaryContract,
    parsed_files: &mut HashMap<PathBuf, syn::File>,
    violations: &mut Vec<String>,
) {
    if contract.primitive_kind == PrimitiveKind::Unknown {
        violations.push(format!(
            "{}:{}: unknown primitive kind",
            contract.path, contract.item
        ));
        return;
    }
    let Some(file) = parse_registered_file(input, contract.path, parsed_files, violations) else {
        return;
    };
    let matches = find_items(file, contract.item);
    if matches.len() != 1 {
        violations.push(format!(
            "{}:{}: expected exactly one AST item, found {}",
            contract.path,
            contract.item,
            matches.len()
        ));
        return;
    }
    let item = matches[0];
    let parameter = capability_parameter(item.signature, contract.capability_ident);
    match parameter {
        Some(parameter) if canonical_capability_type(file, parameter, contract.capability_type) => {
        }
        Some(_) => violations.push(format!(
            "{}:{}: capability `{}` must be the canonical by-value `crate::model_control::{}`",
            contract.path, contract.item, contract.capability_ident, contract.capability_type
        )),
        None => violations.push(format!(
            "{}:{}: missing capability parameter `{}`",
            contract.path, contract.item, contract.capability_ident
        )),
    }

    let mut facts = ItemFacts::new(
        contract.capability_ident,
        Some(contract.runtime_route),
        [contract.capability_ident.to_string()]
            .into_iter()
            .collect(),
        canonical_path_aliases(file),
    );
    facts
        .post_builders
        .extend(signature_request_builders(item.signature));
    facts
        .http_clients
        .extend(signature_http_clients(item.signature));
    facts.visit_block(item.block);
    if facts.shadowed {
        violations.push(format!(
            "{}:{}: capability `{}` is shadowed",
            contract.path, contract.item, contract.capability_ident
        ));
    }
    if facts.assigned {
        violations.push(format!(
            "{}:{}: capability `{}` is assigned",
            contract.path, contract.item, contract.capability_ident
        ));
    }
    if facts.builder_flow_invalid {
        violations.push(format!(
            "{}:{}: request builder is shadowed, disconnected, or reassigned to a different flow",
            contract.path, contract.item
        ));
    }
    if facts.macro_sink {
        violations.push(format!(
            "{}:{}: model primitive is macro-wrapped",
            contract.path, contract.item
        ));
    }

    let primitive_count = facts.primitive_count(contract.primitive_kind);
    if primitive_count != contract.occurrences {
        violations.push(format!(
            "{}:{}: expected {} {:?} primitive(s), found {}",
            contract.path,
            contract.item,
            contract.occurrences,
            contract.primitive_kind,
            primitive_count
        ));
    }

    let (direct_uses, allowed_ident_uses) = match contract.consumption {
        CapabilityConsumption::BindHttpSend => (facts.bind_http_send, facts.bind_http_send),
        CapabilityConsumption::BindHttpOrProviderChat => (
            facts.bind_http_send + facts.provider_chat_argument,
            facts.bind_http_send + facts.provider_chat_argument,
        ),
        CapabilityConsumption::ProviderChatArgument => {
            (facts.provider_chat_argument, facts.provider_chat_argument)
        }
        CapabilityConsumption::SendCodexAppServer => {
            (facts.send_codex_app_server, facts.send_codex_app_server)
        }
        CapabilityConsumption::BindCommandSpawn => {
            (facts.bind_command_spawn, facts.bind_command_spawn)
        }
    };
    let expected_direct = match contract.consumption {
        CapabilityConsumption::BindHttpOrProviderChat => 2,
        _ => 1,
    };
    if direct_uses != expected_direct {
        violations.push(format!(
            "{}:{}: capability `{}` has {} direct consume path(s), expected {}",
            contract.path, contract.item, contract.capability_ident, direct_uses, expected_direct
        ));
    }
    if facts.capability_uses != allowed_ident_uses {
        violations.push(format!(
            "{}:{}: capability `{}` is borrowed, displaced, unused, or used outside its registered consume operation (uses={}, direct={})",
            contract.path,
            contract.item,
            contract.capability_ident,
            facts.capability_uses,
            allowed_ident_uses
        ));
    }
    if facts.raw_http_send != 0 || facts.raw_http_execute != 0 {
        violations.push(format!(
            "{}:{}: boundary contains detached raw HTTP execution (send={}, execute={})",
            contract.path, contract.item, facts.raw_http_send, facts.raw_http_execute,
        ));
    }
}

fn canonical_capability_type(file: &syn::File, ty: &Type, expected: &str) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };
    if path.qself.is_some() {
        return false;
    }
    let segments: Vec<_> = path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    if segments == ["crate", "model_control", expected] {
        return !file_defines_type(file, expected);
    }
    segments == [expected]
        && file_imports_model_control_type(file, expected)
        && !file_defines_type(file, expected)
}

fn file_imports_model_control_type(file: &syn::File, expected: &str) -> bool {
    file.items.iter().any(|item| {
        let Item::Use(item_use) = item else {
            return false;
        };
        use_tree_contains(
            &item_use.tree,
            &mut Vec::new(),
            &["crate", "model_control", expected],
        )
    })
}

fn use_tree_contains(tree: &UseTree, prefix: &mut Vec<String>, expected: &[&str]) -> bool {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            let found = use_tree_contains(&path.tree, prefix, expected);
            prefix.pop();
            found
        }
        UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            let found = prefix
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied());
            prefix.pop();
            found
        }
        UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            let found = prefix
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied())
                && rename.rename == rename.ident;
            prefix.pop();
            found
        }
        UseTree::Group(group) => group
            .items
            .iter()
            .any(|tree| use_tree_contains(tree, prefix, expected)),
        UseTree::Glob(_) => false,
    }
}

fn canonical_path_aliases(file: &syn::File) -> HashMap<String, Vec<String>> {
    let mut aliases = HashMap::new();
    let mut ambiguous = HashSet::new();
    collect_use_aliases(&file.items, &mut aliases, &mut ambiguous);
    collect_type_aliases(&file.items, &mut aliases, &mut ambiguous);
    aliases
}

fn collect_use_aliases(
    items: &[Item],
    aliases: &mut HashMap<String, Vec<String>>,
    ambiguous: &mut HashSet<String>,
) {
    for item in items {
        match item {
            Item::Use(item_use) => {
                collect_use_tree_aliases(&item_use.tree, &mut Vec::new(), aliases, ambiguous);
            }
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_use_aliases(nested, aliases, ambiguous);
                }
            }
            _ => {}
        }
    }
}

fn collect_use_tree_aliases(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    aliases: &mut HashMap<String, Vec<String>>,
    ambiguous: &mut HashSet<String>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_tree_aliases(&path.tree, prefix, aliases, ambiguous);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let mut canonical = prefix.clone();
            canonical.push(name.ident.to_string());
            insert_canonical_alias(name.ident.to_string(), canonical, aliases, ambiguous);
        }
        UseTree::Rename(rename) => {
            let mut canonical = prefix.clone();
            canonical.push(rename.ident.to_string());
            insert_canonical_alias(rename.rename.to_string(), canonical, aliases, ambiguous);
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                collect_use_tree_aliases(tree, prefix, aliases, ambiguous);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn collect_type_aliases(
    items: &[Item],
    aliases: &mut HashMap<String, Vec<String>>,
    ambiguous: &mut HashSet<String>,
) {
    for item in items {
        match item {
            Item::Type(alias) => {
                if let Some(canonical) = resolved_type_path_segments(&alias.ty, aliases) {
                    insert_canonical_alias(alias.ident.to_string(), canonical, aliases, ambiguous);
                }
            }
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_type_aliases(nested, aliases, ambiguous);
                }
            }
            _ => {}
        }
    }
}

fn insert_canonical_alias(
    local: String,
    canonical: Vec<String>,
    aliases: &mut HashMap<String, Vec<String>>,
    ambiguous: &mut HashSet<String>,
) {
    if ambiguous.contains(&local) {
        return;
    }
    match aliases.get(&local) {
        Some(existing) if existing != &canonical => {
            aliases.remove(&local);
            ambiguous.insert(local);
        }
        Some(_) => {}
        None => {
            aliases.insert(local, canonical);
        }
    }
}

fn file_defines_type(file: &syn::File, expected: &str) -> bool {
    file.items.iter().any(|item| match item {
        Item::Struct(item) => item.ident == expected,
        Item::Enum(item) => item.ident == expected,
        Item::Type(item) => item.ident == expected,
        Item::Union(item) => item.ident == expected,
        Item::Mod(module) => module.content.as_ref().is_some_and(|(_, items)| {
            items.iter().any(|item| match item {
                Item::Struct(item) => item.ident == expected,
                Item::Enum(item) => item.ident == expected,
                Item::Type(item) => item.ident == expected,
                Item::Union(item) => item.ident == expected,
                _ => false,
            })
        }),
        _ => false,
    })
}

fn validate_exclusion(
    input: &ValidationInput,
    contract: &NonInvocationContract,
    parsed_files: &mut HashMap<PathBuf, syn::File>,
    violations: &mut Vec<String>,
) {
    let Some(file) = parse_registered_file(input, contract.path, parsed_files, violations) else {
        return;
    };
    let matches = find_items(file, contract.item);
    if matches.len() != 1 {
        violations.push(format!(
            "{}:{}: expected exactly one exclusion AST item, found {}",
            contract.path,
            contract.item,
            matches.len()
        ));
        return;
    }
    let item = matches[0];
    let mut facts = ItemFacts::new("", None, HashSet::new(), canonical_path_aliases(file));
    facts
        .post_builders
        .extend(signature_request_builders(item.signature));
    facts
        .http_clients
        .extend(signature_http_clients(item.signature));
    facts
        .local_bindings
        .extend(signature_binding_idents(item.signature));
    facts.visit_block(item.block);
    let allowed_loopback_send_macro = contract.allowed_operation
        == AllowedExclusionOperation::LoopbackModelPost
        && facts.loopback_bound_posts == contract.occurrences
        && facts.bound_post_raw_send == contract.occurrences;
    if facts.macro_sink && !allowed_loopback_send_macro {
        violations.push(format!(
            "{}:{}: exclusion contains a macro-wrapped sink",
            contract.path, contract.item
        ));
    }
    let proof_present = match contract.allowed_operation {
        AllowedExclusionOperation::HttpGetProbe => {
            operation_proof_count(&facts.get_operation_proofs, contract.proof_ident, false)
                == contract.occurrences
        }
        AllowedExclusionOperation::NonModelHttpPost => {
            direct_operation_ident_count(&facts.post_operation_idents, contract.proof_ident)
                == contract.occurrences
                && !facts.local_bindings.contains(contract.proof_ident)
                && !facts.assigned_bindings.contains(contract.proof_ident)
                && validate_non_model_post_constant(
                    contract.path,
                    contract.item,
                    item.module_items,
                    contract.proof_ident,
                    violations,
                )
        }
        _ => facts
            .operation_proofs
            .iter()
            .any(|proof| proof == contract.proof_ident || proof.contains(contract.proof_ident)),
    };
    let no_model_family =
        facts.provider_chat == 0 && facts.appserver_calls == 0 && facts.raw_http_execute == 0;
    let allowed = proof_present
        && !facts.builder_flow_invalid
        && match contract.allowed_operation {
            AllowedExclusionOperation::CatalogOnly => {
                facts.post == 0 && facts.spawn == 0 && facts.raw_http_send == 0 && no_model_family
            }
            AllowedExclusionOperation::HttpGetProbe => {
                facts.get == contract.occurrences
                    && facts.bound_get_raw_send == contract.occurrences
                    && facts.post == 0
                    && facts.spawn == 0
                    && no_model_family
            }
            AllowedExclusionOperation::TcpProbe => {
                facts.tcp_connect == contract.occurrences
                    && facts.post == 0
                    && facts.spawn == 0
                    && facts.raw_http_send == 0
                    && no_model_family
            }
            AllowedExclusionOperation::TransportSpawn => {
                facts.spawn == contract.occurrences
                    && facts.transport_spawn == contract.occurrences
                    && facts.post == 0
                    && facts.raw_http_send == 0
                    && no_model_family
            }
            AllowedExclusionOperation::LoopbackModelPost => {
                facts.post == contract.occurrences
                    && facts.loopback_bound_posts == contract.occurrences
                    && facts.bound_post_raw_send == contract.occurrences
                    && facts.spawn == 0
                    && no_model_family
            }
            AllowedExclusionOperation::NonModelHttpPost => {
                facts.post == contract.occurrences
                    && facts.bound_post_raw_send == contract.occurrences
                    && !facts.model_endpoint
                    && facts.spawn == 0
                    && no_model_family
            }
            AllowedExclusionOperation::QueueNoOp => {
                facts.post == 0
                    && facts.spawn == 0
                    && facts.raw_http_send == 0
                    && no_model_family
                    && queue_noop_shape(item.block)
            }
        };
    if !allowed {
        violations.push(format!(
            "{}:{}: exclusion predicate {:?} failed (proof={}, post={}, get={}, spawn={}, chat={}, appserver={}, raw_send={}, bound_post={}, bound_get={}, loopback_bound={}, transport_bound={}, tcp_connect={})",
            contract.path,
            contract.item,
            contract.allowed_operation,
            proof_present,
            facts.post,
            facts.get,
            facts.spawn,
            facts.provider_chat,
            facts.appserver_calls,
            facts.raw_http_send,
            facts.bound_post_raw_send,
            facts.bound_get_raw_send,
            facts.loopback_bound_posts,
            facts.transport_spawn,
            facts.tcp_connect,
        ));
    }
}

fn operation_proof_count(
    operations: &[HashSet<String>],
    proof_ident: &str,
    allow_identifier: bool,
) -> usize {
    operations
        .iter()
        .filter(|proofs| {
            proofs.iter().any(|proof| {
                if let Some(ident) = proof.strip_prefix("ident:") {
                    return allow_identifier && ident == proof_ident;
                }
                let literal_or_macro = proof
                    .strip_prefix("literal:")
                    .or_else(|| proof.strip_prefix("macro:"));
                literal_or_macro.is_some_and(|value| {
                    (!allow_identifier
                        || !proof_ident
                            .chars()
                            .all(|character| character == '_' || character.is_ascii_alphanumeric()))
                        && value.contains(proof_ident)
                })
            })
        })
        .count()
}

fn direct_operation_ident_count(operations: &[Option<String>], proof_ident: &str) -> usize {
    operations
        .iter()
        .filter(|ident| ident.as_deref() == Some(proof_ident))
        .count()
}

fn validate_non_model_post_constant(
    path: &str,
    item: &str,
    module_items: &[Item],
    proof_ident: &str,
    violations: &mut Vec<String>,
) -> bool {
    let constants: Vec<_> = module_items
        .iter()
        .filter_map(|candidate| match candidate {
            Item::Const(definition) if definition.ident == proof_ident => Some(definition),
            _ => None,
        })
        .collect();
    let static_count = module_items
        .iter()
        .filter(|candidate| {
            matches!(candidate, Item::Static(definition) if definition.ident == proof_ident)
        })
        .count();
    if constants.len() != 1 || static_count != 0 {
        violations.push(format!(
            "{path}:{item}: non-model POST proof `{proof_ident}` must resolve to exactly one immutable module-level const (consts={}, statics={static_count})",
            constants.len(),
        ));
        return false;
    }

    let definition = constants[0];
    if !type_is_immutable_str_reference(&definition.ty) {
        violations.push(format!(
            "{path}:{item}: non-model POST proof `{proof_ident}` must have type `&str`"
        ));
        return false;
    }
    let Expr::Lit(literal) = definition.expr.as_ref() else {
        violations.push(format!(
            "{path}:{item}: non-model POST proof `{proof_ident}` must be a direct string literal"
        ));
        return false;
    };
    let syn::Lit::Str(url) = &literal.lit else {
        violations.push(format!(
            "{path}:{item}: non-model POST proof `{proof_ident}` must be a string literal"
        ));
        return false;
    };
    if is_model_family_destination(&url.value()) {
        violations.push(format!(
            "{path}:{item}: non-model POST proof `{proof_ident}` resolves to a model-family destination"
        ));
        return false;
    }
    true
}

fn type_is_immutable_str_reference(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Reference(reference)
            if reference.mutability.is_none()
                && matches!(
                    reference.elem.as_ref(),
                    Type::Path(path) if path.qself.is_none() && path.path.is_ident("str")
                )
    )
}

fn queue_noop_shape(block: &Block) -> bool {
    if block.stmts.len() != 2 {
        return false;
    }
    matches!(&block.stmts[0], syn::Stmt::Macro(statement) if statement.mac.path.segments.last().is_some_and(|segment| segment.ident == "debug"))
        && matches!(
            &block.stmts[1],
            syn::Stmt::Expr(Expr::Call(call), None)
                if matches!(
                    call.func.as_ref(),
                    Expr::Path(path)
                        if path.path.segments.last().is_some_and(|segment| segment.ident == "Ok")
                )
        )
}

fn parse_registered_file<'a>(
    input: &ValidationInput,
    relative_path: &str,
    parsed_files: &'a mut HashMap<PathBuf, syn::File>,
    violations: &mut Vec<String>,
) -> Option<&'a syn::File> {
    let path = input.repo_root.join(relative_path);
    if !parsed_files.contains_key(&path) {
        match fs::read_to_string(&path)
            .map_err(|error| format!("failed to read: {error}"))
            .and_then(|source| syn::parse_file(&source).map_err(|error| error.to_string()))
        {
            Ok(file) => {
                parsed_files.insert(path.clone(), file);
            }
            Err(error) => {
                violations.push(format!("{relative_path}: {error}"));
                return None;
            }
        }
    }
    parsed_files.get(&path)
}

#[derive(Clone, Copy)]
struct AstItem<'a> {
    signature: &'a Signature,
    block: &'a Block,
    module_items: &'a [Item],
}

fn find_items<'a>(file: &'a syn::File, selector: &str) -> Vec<AstItem<'a>> {
    let mut found = Vec::new();
    find_items_in(&file.items, selector, false, &mut found);
    found
}

fn find_items_in<'a>(
    items: &'a [Item],
    selector: &str,
    inherited_test: bool,
    found: &mut Vec<AstItem<'a>>,
) {
    for item in items {
        let item_test = inherited_test || item_is_test(item);
        match item {
            Item::Fn(function) if !item_test && function.sig.ident == selector => {
                found.push(AstItem {
                    signature: &function.sig,
                    block: &function.block,
                    module_items: items,
                });
            }
            Item::Impl(item_impl) if !item_test => {
                find_impl_items(item_impl, items, selector, found);
            }
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    find_items_in(nested, selector, item_test, found);
                }
            }
            _ => {}
        }
    }
}

fn find_impl_items<'a>(
    item_impl: &'a ItemImpl,
    module_items: &'a [Item],
    selector: &str,
    found: &mut Vec<AstItem<'a>>,
) {
    let Some((type_name, method_name)) = selector.split_once("::") else {
        return;
    };
    let Type::Path(self_type) = item_impl.self_ty.as_ref() else {
        return;
    };
    if !self_type
        .path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == type_name)
    {
        return;
    }
    for item in &item_impl.items {
        if let ImplItem::Fn(method) = item
            && !has_cfg_test(&method.attrs)
            && method.sig.ident == method_name
        {
            found.push(AstItem {
                signature: &method.sig,
                block: &method.block,
                module_items,
            });
        }
    }
}

fn item_is_test(item: &Item) -> bool {
    match item {
        Item::Fn(item) => has_cfg_test(&item.attrs),
        Item::Impl(item) => has_cfg_test(&item.attrs),
        Item::Mod(item) => item.ident == "tests" || has_cfg_test(&item.attrs),
        _ => false,
    }
}

fn has_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("test")
            || (attr.path().is_ident("cfg")
                && matches!(
                    &attr.meta,
                    syn::Meta::List(list) if list.tokens.to_string().contains("test")
                ))
    })
}

fn capability_parameter<'a>(signature: &'a Signature, name: &str) -> Option<&'a Type> {
    signature.inputs.iter().find_map(|argument| match argument {
        FnArg::Typed(argument) => match argument.pat.as_ref() {
            Pat::Ident(ident) if ident.ident == name => Some(argument.ty.as_ref()),
            _ => None,
        },
        FnArg::Receiver(_) => None,
    })
}

fn signature_request_builders(signature: &Signature) -> HashSet<String> {
    signature_parameters_with_type(signature, "RequestBuilder")
}

fn signature_binding_idents(signature: &Signature) -> HashSet<String> {
    signature
        .inputs
        .iter()
        .filter_map(|argument| {
            let FnArg::Typed(argument) = argument else {
                return None;
            };
            let Pat::Ident(ident) = argument.pat.as_ref() else {
                return None;
            };
            Some(ident.ident.to_string())
        })
        .collect()
}

fn signature_http_clients(signature: &Signature) -> HashSet<String> {
    signature_parameters_with_type(signature, "Client")
}

fn signature_parameters_with_type(signature: &Signature, expected: &str) -> HashSet<String> {
    signature
        .inputs
        .iter()
        .filter_map(|argument| {
            let FnArg::Typed(argument) = argument else {
                return None;
            };
            let Pat::Ident(ident) = argument.pat.as_ref() else {
                return None;
            };
            type_has_terminal_ident(&argument.ty, expected).then(|| ident.ident.to_string())
        })
        .collect()
}

fn type_has_terminal_ident(ty: &Type, expected: &str) -> bool {
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == expected),
        Type::Reference(reference) => type_has_terminal_ident(&reference.elem, expected),
        Type::Paren(paren) => type_has_terminal_ident(&paren.elem, expected),
        Type::Group(group) => type_has_terminal_ident(&group.elem, expected),
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DiscoveredSink {
    path: String,
    item: String,
    kind: PrimitiveKind,
    count: usize,
    typed_delegation: bool,
}

fn discover_repository_sinks(
    input: &ValidationInput,
    parsed_files: &mut HashMap<PathBuf, syn::File>,
    violations: &mut Vec<String>,
) -> Vec<DiscoveredSink> {
    let mut rust_paths = Vec::new();
    if let Err(error) = collect_rust_paths(&input.source_root, &mut rust_paths) {
        violations.push(error);
        return Vec::new();
    }
    let mut discovered = Vec::new();
    for path in rust_paths {
        let relative = relative(&input.repo_root, &path);
        if relative.ends_with("model_control/validator.rs")
            || relative.ends_with("bin/rsi-model-control-validate.rs")
        {
            continue;
        }
        if !parsed_files.contains_key(&path) {
            match fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|source| syn::parse_file(&source).map_err(|error| error.to_string()))
            {
                Ok(file) => {
                    parsed_files.insert(path.clone(), file);
                }
                Err(error) => {
                    violations.push(format!("{relative}: failed to parse: {error}"));
                    continue;
                }
            }
        }
        if let Some(file) = parsed_files.get(&path) {
            discover_items(
                file,
                &file.items,
                &relative,
                false,
                input,
                &mut discovered,
                violations,
            );
        }
    }
    discovered
}

fn discover_items(
    file: &syn::File,
    items: &[Item],
    path: &str,
    inherited_test: bool,
    input: &ValidationInput,
    discovered: &mut Vec<DiscoveredSink>,
    violations: &mut Vec<String>,
) {
    for item in items {
        let item_test = inherited_test || item_is_test(item);
        match item {
            Item::Fn(function) if !item_test => {
                discover_block(
                    file,
                    path,
                    &function.sig.ident.to_string(),
                    &function.sig,
                    &function.block,
                    input,
                    discovered,
                    violations,
                );
            }
            Item::Impl(item_impl) if !item_test => {
                let type_name = match item_impl.self_ty.as_ref() {
                    Type::Path(path) => path
                        .path
                        .segments
                        .last()
                        .map(|segment| segment.ident.to_string()),
                    _ => None,
                };
                if let Some(type_name) = type_name {
                    for impl_item in &item_impl.items {
                        if let ImplItem::Fn(method) = impl_item
                            && !has_cfg_test(&method.attrs)
                        {
                            discover_block(
                                file,
                                path,
                                &format!("{type_name}::{}", method.sig.ident),
                                &method.sig,
                                &method.block,
                                input,
                                discovered,
                                violations,
                            );
                        }
                    }
                }
            }
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    discover_items(file, nested, path, item_test, input, discovered, violations);
                }
            }
            _ => {}
        }
    }
}

fn discover_block(
    file: &syn::File,
    path: &str,
    item: &str,
    signature: &Signature,
    block: &Block,
    input: &ValidationInput,
    discovered: &mut Vec<DiscoveredSink>,
    violations: &mut Vec<String>,
) {
    let known_capabilities = signature
        .inputs
        .iter()
        .filter_map(|argument| {
            let FnArg::Typed(argument) = argument else {
                return None;
            };
            let Pat::Ident(ident) = argument.pat.as_ref() else {
                return None;
            };
            canonical_capability_type(file, &argument.ty, "ModelExecutionCapability")
                .then(|| ident.ident.to_string())
        })
        .collect();
    let mut facts = ItemFacts::new("", None, known_capabilities, canonical_path_aliases(file));
    facts
        .post_builders
        .extend(signature_request_builders(signature));
    facts.http_clients.extend(signature_http_clients(signature));
    facts.visit_block(block);
    let allowed_loopback_send_macro = input.exclusions.iter().any(|contract| {
        contract.path == path
            && contract.item == item
            && contract.allowed_operation == AllowedExclusionOperation::LoopbackModelPost
            && facts.loopback_bound_posts == contract.occurrences
            && facts.bound_post_raw_send == contract.occurrences
    });
    if facts.macro_sink && !allowed_loopback_send_macro {
        violations.push(format!(
            "{path}:{item}: macro-wrapped model sink is not allowed"
        ));
    }
    for (kind, count) in [
        (PrimitiveKind::HttpPost, facts.post),
        (PrimitiveKind::CliSpawn, facts.spawn),
        (PrimitiveKind::ProviderChat, facts.provider_chat),
        (PrimitiveKind::AppServerModelRequest, facts.appserver_calls),
        (
            PrimitiveKind::RawHttpSend,
            usize::from(
                facts.raw_http_send + facts.raw_http_execute > 0
                    && facts.post == 0
                    && facts.get == 0
                    && !(path.ends_with("model_control/mod.rs")
                        && item == "AdmittedHttpRequest::send"),
            ),
        ),
    ] {
        if count > 0 {
            discovered.push(DiscoveredSink {
                path: path.to_string(),
                item: item.to_string(),
                kind,
                count,
                typed_delegation: kind == PrimitiveKind::ProviderChat
                    && facts.provider_chat == facts.typed_provider_chat,
            });
        }
    }
}

fn validate_closed_join(
    input: &ValidationInput,
    discovered: &[DiscoveredSink],
    violations: &mut Vec<String>,
) {
    for sink in discovered {
        let boundary_matches = input
            .boundaries
            .iter()
            .filter(|contract| {
                contract.path == sink.path
                    && contract.item == sink.item
                    && contract.primitive_kind == sink.kind
            })
            .count();
        let exclusion_matches = input
            .exclusions
            .iter()
            .filter(|contract| {
                contract.path == sink.path
                    && contract.item == sink.item
                    && exclusion_covers(contract.allowed_operation, sink.kind)
            })
            .count();
        let compiler_closed_delegation = sink.kind == PrimitiveKind::ProviderChat
            && sink.typed_delegation
            && boundary_matches + exclusion_matches == 0;
        if boundary_matches + exclusion_matches != 1 && !compiler_closed_delegation {
            violations.push(format!(
                "{}:{}: {:?} sink joins {} contracts, expected exactly one",
                sink.path,
                sink.item,
                sink.kind,
                boundary_matches + exclusion_matches
            ));
        }
    }

    for contract in &input.boundaries {
        if matches!(
            contract.primitive_kind,
            PrimitiveKind::HttpPost
                | PrimitiveKind::CliSpawn
                | PrimitiveKind::ProviderChat
                | PrimitiveKind::AppServerModelRequest
        ) && !discovered.iter().any(|sink| {
            sink.path == contract.path
                && sink.item == contract.item
                && sink.kind == contract.primitive_kind
        }) {
            violations.push(format!(
                "{}:{}: registered {:?} boundary was not discovered",
                contract.path, contract.item, contract.primitive_kind
            ));
        }
    }
}

fn exclusion_covers(operation: AllowedExclusionOperation, kind: PrimitiveKind) -> bool {
    matches!(
        (operation, kind),
        (
            AllowedExclusionOperation::TransportSpawn,
            PrimitiveKind::CliSpawn
        ) | (
            AllowedExclusionOperation::LoopbackModelPost,
            PrimitiveKind::HttpPost
        ) | (
            AllowedExclusionOperation::NonModelHttpPost,
            PrimitiveKind::HttpPost
        )
    )
}

fn collect_rust_paths(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in
        fs::read_dir(root).map_err(|error| format!("failed to read {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("failed to read directory entry: {error}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_rust_paths(&path, paths)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            paths.push(path);
        }
    }
    Ok(())
}

struct ItemFacts<'a> {
    capability_ident: &'a str,
    expected_route: Option<registry::RuntimeExecutionRoute>,
    known_capability_idents: HashSet<String>,
    canonical_path_aliases: HashMap<String, Vec<String>>,
    post_builders: HashSet<String>,
    get_builders: HashSet<String>,
    process_commands: HashSet<String>,
    http_clients: HashSet<String>,
    transport_commands: HashSet<String>,
    value_proofs: HashMap<String, HashSet<String>>,
    appserver_payloads: HashSet<String>,
    local_bindings: HashSet<String>,
    assigned_bindings: HashSet<String>,
    capability_uses: usize,
    shadowed: bool,
    assigned: bool,
    builder_flow_invalid: bool,
    post: usize,
    get: usize,
    spawn: usize,
    provider_chat: usize,
    send_codex_app_server: usize,
    appserver_calls: usize,
    bind_http_send: usize,
    bind_command_spawn: usize,
    provider_chat_argument: usize,
    typed_provider_chat: usize,
    raw_http_send: usize,
    raw_http_execute: usize,
    bound_post_raw_send: usize,
    bound_get_raw_send: usize,
    transport_spawn: usize,
    ollama_url_calls: usize,
    loopback_bound_posts: usize,
    post_operation_proofs: Vec<HashSet<String>>,
    post_operation_idents: Vec<Option<String>>,
    get_operation_proofs: Vec<HashSet<String>>,
    operation_proofs: HashSet<String>,
    tcp_connect: usize,
    macro_sink: bool,
    model_endpoint: bool,
    model_executable: bool,
}

impl<'a> ItemFacts<'a> {
    fn new(
        capability_ident: &'a str,
        expected_route: Option<registry::RuntimeExecutionRoute>,
        known_capability_idents: HashSet<String>,
        canonical_path_aliases: HashMap<String, Vec<String>>,
    ) -> Self {
        Self {
            capability_ident,
            expected_route,
            known_capability_idents,
            canonical_path_aliases,
            post_builders: HashSet::new(),
            get_builders: HashSet::new(),
            process_commands: HashSet::new(),
            http_clients: HashSet::new(),
            transport_commands: HashSet::new(),
            value_proofs: HashMap::new(),
            appserver_payloads: HashSet::new(),
            local_bindings: HashSet::new(),
            assigned_bindings: HashSet::new(),
            capability_uses: 0,
            shadowed: false,
            assigned: false,
            builder_flow_invalid: false,
            post: 0,
            get: 0,
            spawn: 0,
            provider_chat: 0,
            send_codex_app_server: 0,
            appserver_calls: 0,
            bind_http_send: 0,
            bind_command_spawn: 0,
            provider_chat_argument: 0,
            typed_provider_chat: 0,
            raw_http_send: 0,
            raw_http_execute: 0,
            bound_post_raw_send: 0,
            bound_get_raw_send: 0,
            transport_spawn: 0,
            ollama_url_calls: 0,
            loopback_bound_posts: 0,
            post_operation_proofs: Vec::new(),
            post_operation_idents: Vec::new(),
            get_operation_proofs: Vec::new(),
            operation_proofs: HashSet::new(),
            tcp_connect: 0,
            macro_sink: false,
            model_endpoint: false,
            model_executable: false,
        }
    }

    fn primitive_count(&self, kind: PrimitiveKind) -> usize {
        match kind {
            PrimitiveKind::CliSpawn => self.spawn,
            PrimitiveKind::HttpPost => self.post,
            PrimitiveKind::RawHttpSend => self.raw_http_send + self.raw_http_execute,
            PrimitiveKind::ProviderChat => self.provider_chat,
            PrimitiveKind::AppServerModelRequest => self.appserver_calls,
            PrimitiveKind::Unknown => 0,
        }
    }

    fn record_macro_tokens(&mut self, tokens: String) {
        self.operation_proofs.insert(tokens.clone());
        let compact: String = tokens
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let post = compact.contains(".post(")
            || compact.contains(".request(Method::POST")
            || compact.contains("Request::new(Method::POST")
            || compact.contains("Client::post(");
        let provider = compact.contains(".chat(") || compact.contains(".stream_chat(");
        let typed_provider =
            provider && macro_provider_calls_are_typed(&compact, &self.known_capability_idents);
        let execute = [
            "client.execute(",
            "http.execute(",
            "Client::execute(",
            "Client::new().execute(",
        ]
        .iter()
        .any(|needle| compact.contains(needle))
            || self
                .http_clients
                .iter()
                .any(|client| compact.contains(&format!("{client}.execute(")));
        let request_send = self
            .post_builders
            .iter()
            .chain(&self.get_builders)
            .any(|builder| compact.contains(&format!("{builder}.send(")));
        let appserver_send = compact.contains(".send(")
            && (compact.contains("thread/start")
                || compact.contains("turn/start")
                || self
                    .appserver_payloads
                    .iter()
                    .any(|payload| compact.contains(payload)));
        let associated_send =
            compact.contains("RequestBuilder::send(") || compact.contains("Sender::send(");
        let associated_spawn = compact.contains("Command::spawn(");
        if post
            || (provider && !typed_provider)
            || execute
            || request_send
            || appserver_send
            || associated_send
            || associated_spawn
            || (compact.contains(".spawn(")
                && (compact.contains("Command")
                    || compact.contains("bind_command")
                    || self
                        .process_commands
                        .iter()
                        .any(|command| compact.contains(command))))
            || compact.contains("send_codex_app_server")
        {
            self.macro_sink = true;
        }
        if request_send {
            self.raw_http_send = self.raw_http_send.max(1);
            self.bound_post_raw_send = self.bound_post_raw_send.max(1);
        }
    }
}

impl<'ast> Visit<'ast> for ItemFacts<'_> {
    fn visit_local(&mut self, node: &'ast Local) {
        if let Some(init) = &node.init {
            if let Pat::Ident(ident) = &node.pat {
                let name = ident.ident.to_string();
                self.local_bindings.insert(name.clone());
                let aliases_builder = self
                    .post_builders
                    .iter()
                    .chain(&self.get_builders)
                    .any(|builder| expression_is_receiver_flow_from(&init.expr, builder));
                let was_builder =
                    self.post_builders.remove(&name) || self.get_builders.remove(&name);
                if was_builder || aliases_builder {
                    self.builder_flow_invalid = true;
                }
                if expression_is_post_builder(
                    &init.expr,
                    &self.post_builders,
                    &self.canonical_path_aliases,
                ) {
                    self.post_builders.insert(name.clone());
                }
                if expression_is_get_builder(&init.expr, &self.get_builders, &self.http_clients) {
                    self.get_builders.insert(name.clone());
                }
                if expression_constructs_process(&init.expr) {
                    self.process_commands.insert(name.clone());
                }
                if expression_constructs_http_client(&init.expr) {
                    self.http_clients.insert(name.clone());
                }
                let proofs = expression_operation_proofs(&init.expr, &self.value_proofs);
                if expression_is_appserver_payload(&proofs)
                    && expression_serializes_payload(&init.expr)
                {
                    self.appserver_payloads.insert(name.clone());
                }
                self.value_proofs.insert(name, proofs);
            }
            if expression_ends_with_method(&init.expr, "into_parts") {
                collect_execution_bindings(&node.pat, &mut self.known_capability_idents);
            }
        }
        visit::visit_local(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast ExprPath) {
        if path_is_ident(node, self.capability_ident) {
            self.capability_uses += 1;
        }
        if let Some(segment) = node.path.segments.last() {
            self.operation_proofs.insert(segment.ident.to_string());
        }
        visit::visit_expr_path(self, node);
    }

    fn visit_pat_ident(&mut self, node: &'ast PatIdent) {
        self.local_bindings.insert(node.ident.to_string());
        if !self.capability_ident.is_empty() && node.ident == self.capability_ident {
            self.shadowed = true;
        }
        visit::visit_pat_ident(self, node);
    }

    fn visit_expr_assign(&mut self, node: &'ast ExprAssign) {
        if expr_is_ident(&node.left, self.capability_ident) {
            self.assigned = true;
        }
        if let Some(name) = expression_ident(&node.left) {
            self.assigned_bindings.insert(name.clone());
            let was_post = self.post_builders.contains(&name);
            let was_get = self.get_builders.contains(&name);
            let proofs = expression_operation_proofs(&node.right, &self.value_proofs);
            if was_post || was_get {
                let preserves_identity = expression_is_receiver_flow_from(&node.right, &name);
                let remains_post = was_post
                    && expression_is_post_builder(
                        &node.right,
                        &self.post_builders,
                        &self.canonical_path_aliases,
                    );
                let remains_get = was_get
                    && expression_is_get_builder(
                        &node.right,
                        &self.get_builders,
                        &self.http_clients,
                    );
                if !preserves_identity || (!remains_post && !remains_get) {
                    self.builder_flow_invalid = true;
                    self.post_builders.remove(&name);
                    self.get_builders.remove(&name);
                }
            }
            self.value_proofs.insert(name, proofs);
        }
        visit::visit_expr_assign(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let method = node.method.to_string();
        self.operation_proofs.insert(method.clone());
        match method.as_str() {
            "post" => {
                self.post += 1;
                if let Some(url) = node.args.first() {
                    self.post_operation_proofs
                        .push(expression_operation_proofs(url, &self.value_proofs));
                    self.post_operation_idents
                        .push(direct_path_ident(url));
                }
                if node.args.first().is_some_and(expression_calls_ollama_url) {
                    self.loopback_bound_posts += 1;
                }
            }
            "request"
                if node
                    .args
                    .first()
                    .is_some_and(expression_is_http_post_method) =>
            {
                self.post += 1;
                if let Some(url) = node.args.iter().nth(1) {
                    self.post_operation_proofs
                        .push(expression_operation_proofs(url, &self.value_proofs));
                    self.post_operation_idents
                        .push(direct_path_ident(url));
                }
            }
            "get" if receiver_looks_http(node.receiver.as_ref(), &self.http_clients) => {
                self.get += 1;
                if let Some(url) = node.args.first() {
                    self.get_operation_proofs
                        .push(expression_operation_proofs(url, &self.value_proofs));
                }
            }
            "arg"
                if node
                    .args
                    .first()
                    .is_some_and(|argument| {
                        matches!(argument, Expr::Lit(lit) if matches!(&lit.lit, syn::Lit::Str(value) if value.value() == "app-server"))
                    }) =>
            {
                if let Some(command) = expression_ident(&node.receiver) {
                    self.transport_commands.insert(command);
                }
            }
            "spawn"
                if receiver_looks_process(node.receiver.as_ref(), &self.process_commands) =>
            {
                self.spawn += 1;
                if let Expr::MethodCall(bind) = node.receiver.as_ref()
                    && bind.method == "bind_command"
                    && expr_is_ident(&bind.receiver, self.capability_ident)
                    && bind.args.len() == 2
                    && self
                        .expected_route
                        .is_some_and(|route| expression_is_runtime_route(&bind.args[0], route))
                {
                    self.bind_command_spawn += 1;
                }
                if expression_ident(&node.receiver)
                    .is_some_and(|command| self.transport_commands.contains(&command))
                {
                    self.transport_spawn += 1;
                }
            }
            "chat" | "stream_chat" => {
                self.provider_chat += 1;
                if node
                    .args
                    .iter()
                    .any(|argument| expr_is_ident(argument, self.capability_ident))
                {
                    self.provider_chat_argument += 1;
                }
                if node.args.iter().any(|argument| {
                    expression_ident(argument).is_some_and(|ident| {
                        self.known_capability_idents.contains(&ident)
                    })
                }) {
                    self.typed_provider_chat += 1;
                }
            }
            "send_codex_app_server" => {
                self.appserver_calls += 1;
                if expr_is_ident(&node.receiver, self.capability_ident) {
                    self.send_codex_app_server += 1;
                }
            }
            "send" => {
                if node.args.is_empty() {
                    self.raw_http_send += 1;
                    if expression_is_post_builder(
                        &node.receiver,
                        &self.post_builders,
                        &self.canonical_path_aliases,
                    ) {
                        self.bound_post_raw_send += 1;
                    }
                    if expression_is_get_builder(
                        &node.receiver,
                        &self.get_builders,
                        &self.http_clients,
                    ) {
                        self.bound_get_raw_send += 1;
                    }
                }
                if !node.args.is_empty()
                    && node.args.iter().any(|argument| {
                        let proofs = expression_operation_proofs(argument, &self.value_proofs);
                        (expression_is_appserver_payload(&proofs)
                            && expression_serializes_payload(argument))
                            || expression_ident(argument)
                                .is_some_and(|ident| self.appserver_payloads.contains(&ident))
                    })
                {
                    self.appserver_calls += 1;
                }
                if let Expr::MethodCall(bind) = node.receiver.as_ref()
                    && bind.method == "bind_http"
                    && expr_is_ident(&bind.receiver, self.capability_ident)
                    && bind_http_matches_registered_route_and_post(
                        bind,
                        self.expected_route,
                        &self.post_builders,
                        &self.canonical_path_aliases,
                    )
                {
                    self.bind_http_send += 1;
                }
            }
            "execute"
                if receiver_looks_http(node.receiver.as_ref(), &self.http_clients)
                    || node.args.first().is_some_and(|argument| {
                        expression_is_post_builder(
                            argument,
                            &self.post_builders,
                            &self.canonical_path_aliases,
                        )
                    }) =>
            {
                self.raw_http_execute += 1;
            }
            _ => {}
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if expression_is_request_new_post(node) {
            self.post += 1;
            if let Some(url) = node.args.iter().nth(1) {
                self.post_operation_proofs
                    .push(expression_operation_proofs(url, &self.value_proofs));
                self.post_operation_idents.push(direct_path_ident(url));
            }
        }
        if expression_is_client_execute(node) {
            self.raw_http_execute += 1;
        }
        match associated_sink_call(node, &self.canonical_path_aliases) {
            Some(AssociatedSinkCall::ClientPost) => {
                self.post += 1;
                if let Some(url) = node.args.iter().nth(1) {
                    self.post_operation_proofs
                        .push(expression_operation_proofs(url, &self.value_proofs));
                    self.post_operation_idents.push(direct_path_ident(url));
                }
            }
            Some(AssociatedSinkCall::RequestBuilderSend) => {
                self.raw_http_send += 1;
                if let Some(request) = node.args.first() {
                    if expression_is_post_builder(
                        request,
                        &self.post_builders,
                        &self.canonical_path_aliases,
                    ) {
                        self.bound_post_raw_send += 1;
                    }
                    if expression_is_get_builder(request, &self.get_builders, &self.http_clients) {
                        self.bound_get_raw_send += 1;
                    }
                }
            }
            Some(AssociatedSinkCall::CommandSpawn) => {
                self.spawn += 1;
                if node
                    .args
                    .first()
                    .and_then(expression_ident)
                    .is_some_and(|command| self.transport_commands.contains(&command))
                {
                    self.transport_spawn += 1;
                }
            }
            Some(AssociatedSinkCall::SenderSend) => {
                if node.args.iter().nth(1).is_some_and(|argument| {
                    let proofs = expression_operation_proofs(argument, &self.value_proofs);
                    (expression_is_appserver_payload(&proofs)
                        && expression_serializes_payload(argument))
                        || expression_ident(argument)
                            .is_some_and(|ident| self.appserver_payloads.contains(&ident))
                }) {
                    self.appserver_calls += 1;
                }
            }
            None => {}
        }
        if let Expr::Path(path) = node.func.as_ref()
            && path.path.segments.last().is_some_and(|segment| {
                let name = segment.ident.to_string();
                self.operation_proofs.insert(name.clone());
                if name == "ollama_url" {
                    self.ollama_url_calls += 1;
                }
                if name == "connect"
                    && path
                        .path
                        .segments
                        .iter()
                        .any(|segment| segment.ident == "TcpStream")
                {
                    self.tcp_connect += 1;
                }
                false
            })
        {}
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_macro(&mut self, node: &'ast ExprMacro) {
        self.record_macro_tokens(node.mac.tokens.to_string());
        visit::visit_expr_macro(self, node);
    }

    fn visit_lit_str(&mut self, node: &'ast LitStr) {
        let literal = node.value();
        self.operation_proofs.insert(literal.clone());
        let value = literal.to_ascii_lowercase();
        if is_model_family_destination(&value) {
            self.model_endpoint = true;
        }
        if ["claude", "codex", "agy", "antigravity", "ollama"]
            .iter()
            .any(|executable| value == *executable)
        {
            self.model_executable = true;
        }
        visit::visit_lit_str(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.record_macro_tokens(node.tokens.to_string());
        visit::visit_macro(self, node);
    }
}

fn is_model_family_destination(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "/chat/completions",
        "/responses",
        "/v1/messages",
        "/embeddings",
        "/api/chat",
        "/api/embed",
        "/api/generate",
        "thread/start",
        "turn/start",
    ]
    .iter()
    .any(|needle| value.contains(needle))
}

fn expr_is_ident(expression: &Expr, ident: &str) -> bool {
    matches!(expression, Expr::Path(path) if path_is_ident(path, ident))
}

fn expression_ident(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Path(path) if path.path.segments.len() == 1 => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        Expr::Reference(reference) => expression_ident(&reference.expr),
        _ => None,
    }
}

fn direct_path_ident(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
            path.path.get_ident().map(ToString::to_string)
        }
        Expr::Paren(paren) => direct_path_ident(&paren.expr),
        Expr::Group(group) => direct_path_ident(&group.expr),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum AssociatedSinkCall {
    ClientPost,
    RequestBuilderSend,
    CommandSpawn,
    SenderSend,
}

fn associated_sink_call(
    call: &syn::ExprCall,
    aliases: &HashMap<String, Vec<String>>,
) -> Option<AssociatedSinkCall> {
    let Expr::Path(path) = call.func.as_ref() else {
        return None;
    };
    let resolved = resolved_expr_path_segments(path, aliases)?;
    match resolved.as_slice() {
        [root, ty, method] if root == "reqwest" && ty == "Client" && method == "post" => {
            Some(AssociatedSinkCall::ClientPost)
        }
        [root, ty, method] if root == "reqwest" && ty == "RequestBuilder" && method == "send" => {
            Some(AssociatedSinkCall::RequestBuilderSend)
        }
        [root, module, ty, method]
            if root == "tokio" && module == "process" && ty == "Command" && method == "spawn" =>
        {
            Some(AssociatedSinkCall::CommandSpawn)
        }
        [root, sync, channel, ty, method]
            if root == "tokio"
                && sync == "sync"
                && channel == "mpsc"
                && ty == "Sender"
                && method == "send" =>
        {
            Some(AssociatedSinkCall::SenderSend)
        }
        _ => None,
    }
}

fn resolved_expr_path_segments(
    path: &ExprPath,
    aliases: &HashMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    if let Some(qself) = &path.qself {
        let mut resolved = resolved_type_path_segments(&qself.ty, aliases)?;
        resolved.extend(
            path.path
                .segments
                .iter()
                .skip(qself.position)
                .map(|segment| segment.ident.to_string()),
        );
        return Some(resolved);
    }
    Some(resolve_path_segments(&path.path, aliases))
}

fn resolved_type_path_segments(
    ty: &Type,
    aliases: &HashMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    match ty {
        Type::Path(path) if path.qself.is_none() => {
            Some(resolve_path_segments(&path.path, aliases))
        }
        Type::Reference(reference) => resolved_type_path_segments(&reference.elem, aliases),
        Type::Paren(paren) => resolved_type_path_segments(&paren.elem, aliases),
        Type::Group(group) => resolved_type_path_segments(&group.elem, aliases),
        _ => None,
    }
}

fn resolve_path_segments(path: &syn::Path, aliases: &HashMap<String, Vec<String>>) -> Vec<String> {
    let segments: Vec<_> = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    let Some((first, remainder)) = segments.split_first() else {
        return segments;
    };
    let Some(prefix) = aliases.get(first) else {
        return segments;
    };
    let mut resolved = prefix.clone();
    resolved.extend(remainder.iter().cloned());
    resolved
}

fn collect_execution_bindings(pattern: &Pat, bindings: &mut HashSet<String>) {
    match pattern {
        Pat::Ident(ident) if ident.ident.to_string().contains("execution") => {
            bindings.insert(ident.ident.to_string());
        }
        Pat::Tuple(tuple) => {
            for element in &tuple.elems {
                collect_execution_bindings(element, bindings);
            }
        }
        Pat::Paren(paren) => collect_execution_bindings(&paren.pat, bindings),
        _ => {}
    }
}

fn expression_ends_with_method(expression: &Expr, method: &str) -> bool {
    match expression {
        Expr::MethodCall(call) => call.method == method,
        Expr::Await(awaited) => expression_ends_with_method(&awaited.base, method),
        Expr::Try(tried) => expression_ends_with_method(&tried.expr, method),
        Expr::Paren(paren) => expression_ends_with_method(&paren.expr, method),
        Expr::Group(group) => expression_ends_with_method(&group.expr, method),
        _ => false,
    }
}

fn expression_is_receiver_flow_from(expression: &Expr, ident: &str) -> bool {
    match expression {
        Expr::Path(path) => path.path.is_ident(ident),
        Expr::MethodCall(call) => {
            !method_terminates_request_builder(&call.method)
                && expression_is_receiver_flow_from(&call.receiver, ident)
        }
        Expr::Await(awaited) => expression_is_receiver_flow_from(&awaited.base, ident),
        Expr::Try(tried) => expression_is_receiver_flow_from(&tried.expr, ident),
        Expr::Paren(paren) => expression_is_receiver_flow_from(&paren.expr, ident),
        Expr::Group(group) => expression_is_receiver_flow_from(&group.expr, ident),
        _ => false,
    }
}

fn macro_provider_calls_are_typed(
    compact_tokens: &str,
    known_capabilities: &HashSet<String>,
) -> bool {
    let mut found = false;
    for method in [".chat(", ".stream_chat("] {
        let mut remainder = compact_tokens;
        while let Some(start) = remainder.find(method) {
            found = true;
            let arguments = &remainder[start + method.len()..];
            let Some(end) = arguments.find(')') else {
                return false;
            };
            if !known_capabilities
                .iter()
                .any(|ident| arguments[..end].contains(ident))
            {
                return false;
            }
            remainder = &arguments[end + 1..];
        }
    }
    found
}

fn expression_is_post_builder(
    expression: &Expr,
    post_builders: &HashSet<String>,
    aliases: &HashMap<String, Vec<String>>,
) -> bool {
    match expression {
        Expr::MethodCall(call) => {
            !method_terminates_request_builder(&call.method)
                && ((call.method == "post")
                    || (call.method == "request"
                        && call
                            .args
                            .first()
                            .is_some_and(expression_is_http_post_method))
                    || expression_is_post_builder(&call.receiver, post_builders, aliases))
        }
        Expr::Call(call) => {
            expression_is_request_new_post(call)
                || matches!(
                    associated_sink_call(call, aliases),
                    Some(AssociatedSinkCall::ClientPost)
                )
        }
        Expr::Path(path) => path
            .path
            .get_ident()
            .is_some_and(|ident| post_builders.contains(&ident.to_string())),
        Expr::Await(awaited) => expression_is_post_builder(&awaited.base, post_builders, aliases),
        Expr::Try(tried) => expression_is_post_builder(&tried.expr, post_builders, aliases),
        Expr::Paren(paren) => expression_is_post_builder(&paren.expr, post_builders, aliases),
        Expr::Group(group) => expression_is_post_builder(&group.expr, post_builders, aliases),
        Expr::Reference(reference) => {
            expression_is_post_builder(&reference.expr, post_builders, aliases)
        }
        _ => false,
    }
}

fn expression_is_get_builder(
    expression: &Expr,
    get_builders: &HashSet<String>,
    http_clients: &HashSet<String>,
) -> bool {
    match expression {
        Expr::MethodCall(call) => {
            !method_terminates_request_builder(&call.method)
                && ((call.method == "get" && receiver_looks_http(&call.receiver, http_clients))
                    || expression_is_get_builder(&call.receiver, get_builders, http_clients))
        }
        Expr::Path(path) => path
            .path
            .get_ident()
            .is_some_and(|ident| get_builders.contains(&ident.to_string())),
        Expr::Await(awaited) => {
            expression_is_get_builder(&awaited.base, get_builders, http_clients)
        }
        Expr::Try(tried) => expression_is_get_builder(&tried.expr, get_builders, http_clients),
        Expr::Paren(paren) => expression_is_get_builder(&paren.expr, get_builders, http_clients),
        Expr::Group(group) => expression_is_get_builder(&group.expr, get_builders, http_clients),
        Expr::Reference(reference) => {
            expression_is_get_builder(&reference.expr, get_builders, http_clients)
        }
        _ => false,
    }
}

fn method_terminates_request_builder(method: &syn::Ident) -> bool {
    matches!(method.to_string().as_str(), "build" | "execute" | "send")
}

fn expression_is_http_post_method(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Path(path)
            if path.path.segments.last().is_some_and(|segment| segment.ident == "POST")
    )
}

fn expression_is_request_new_post(call: &syn::ExprCall) -> bool {
    matches!(
        call.func.as_ref(),
        Expr::Path(path)
            if path.path.segments.last().is_some_and(|segment| segment.ident == "new")
                && path.path.segments.iter().any(|segment| segment.ident == "Request")
                && call.args.first().is_some_and(expression_is_http_post_method)
    )
}

fn expression_is_client_execute(call: &syn::ExprCall) -> bool {
    matches!(
        call.func.as_ref(),
        Expr::Path(path)
            if path.path.segments.last().is_some_and(|segment| segment.ident == "execute")
                && path.path.segments.iter().any(|segment| segment.ident == "Client")
    )
}

fn expression_constructs_process(expression: &Expr) -> bool {
    match expression {
        Expr::Call(call) => matches!(
            call.func.as_ref(),
            Expr::Path(path)
                if path.path.segments.iter().any(|segment| segment.ident == "Command")
                    && path.path.segments.last().is_some_and(|segment| segment.ident == "new")
        ),
        Expr::MethodCall(call) => expression_constructs_process(&call.receiver),
        Expr::Await(awaited) => expression_constructs_process(&awaited.base),
        Expr::Try(tried) => expression_constructs_process(&tried.expr),
        Expr::Paren(paren) => expression_constructs_process(&paren.expr),
        Expr::Group(group) => expression_constructs_process(&group.expr),
        _ => false,
    }
}

fn expression_constructs_http_client(expression: &Expr) -> bool {
    match expression {
        Expr::Call(call) => matches!(
            call.func.as_ref(),
            Expr::Path(path)
                if path.path.segments.iter().any(|segment| segment.ident == "Client")
                    && path.path.segments.last().is_some_and(|segment| {
                        segment.ident == "new" || segment.ident == "builder"
                    })
        ),
        Expr::MethodCall(call) => expression_constructs_http_client(&call.receiver),
        Expr::Await(awaited) => expression_constructs_http_client(&awaited.base),
        Expr::Try(tried) => expression_constructs_http_client(&tried.expr),
        Expr::Paren(paren) => expression_constructs_http_client(&paren.expr),
        Expr::Group(group) => expression_constructs_http_client(&group.expr),
        _ => false,
    }
}

fn expression_operation_proofs(
    expression: &Expr,
    value_proofs: &HashMap<String, HashSet<String>>,
) -> HashSet<String> {
    struct ProofVisitor<'a> {
        value_proofs: &'a HashMap<String, HashSet<String>>,
        proofs: HashSet<String>,
    }

    impl<'ast> Visit<'ast> for ProofVisitor<'_> {
        fn visit_expr_path(&mut self, node: &'ast ExprPath) {
            if let Some(segment) = node.path.segments.last() {
                let ident = segment.ident.to_string();
                self.proofs.insert(format!("ident:{ident}"));
                if let Some(proofs) = self.value_proofs.get(&ident) {
                    self.proofs.extend(proofs.iter().cloned());
                }
            }
            visit::visit_expr_path(self, node);
        }

        fn visit_lit_str(&mut self, node: &'ast LitStr) {
            self.proofs.insert(format!("literal:{}", node.value()));
            visit::visit_lit_str(self, node);
        }

        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            let tokens = node.tokens.to_string();
            self.proofs.insert(format!("macro:{tokens}"));
            for (ident, proofs) in self.value_proofs {
                if tokens.contains(ident) {
                    self.proofs.extend(proofs.iter().cloned());
                }
            }
            visit::visit_macro(self, node);
        }
    }

    let mut visitor = ProofVisitor {
        value_proofs,
        proofs: HashSet::new(),
    };
    visitor.visit_expr(expression);
    visitor.proofs
}

fn expression_is_appserver_payload(proofs: &HashSet<String>) -> bool {
    proofs.iter().any(|proof| {
        let proof = proof.to_ascii_lowercase();
        proof.contains("thread/start") || proof.contains("turn/start")
    })
}

fn expression_serializes_payload(expression: &Expr) -> bool {
    match expression {
        Expr::MethodCall(call) => {
            matches!(
                call.method.to_string().as_str(),
                "into_bytes" | "as_bytes" | "to_vec"
            ) || expression_serializes_payload(&call.receiver)
        }
        Expr::Call(call) => {
            matches!(
                call.func.as_ref(),
                Expr::Path(path)
                    if path.path.segments.last().is_some_and(|segment| {
                        matches!(
                            segment.ident.to_string().as_str(),
                            "serialize" | "to_string" | "to_vec"
                        )
                    })
            ) || call.args.iter().any(expression_serializes_payload)
        }
        Expr::Macro(expression) => expression
            .mac
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "format"),
        Expr::Await(awaited) => expression_serializes_payload(&awaited.base),
        Expr::Try(tried) => expression_serializes_payload(&tried.expr),
        Expr::Paren(paren) => expression_serializes_payload(&paren.expr),
        Expr::Group(group) => expression_serializes_payload(&group.expr),
        Expr::Reference(reference) => expression_serializes_payload(&reference.expr),
        _ => false,
    }
}

fn expression_calls_ollama_url(expression: &Expr) -> bool {
    match expression {
        Expr::Call(call) => matches!(
            call.func.as_ref(),
            Expr::Path(path)
                if path.path.segments.last().is_some_and(|segment| segment.ident == "ollama_url")
        ),
        Expr::Try(tried) => expression_calls_ollama_url(&tried.expr),
        Expr::Paren(paren) => expression_calls_ollama_url(&paren.expr),
        Expr::Group(group) => expression_calls_ollama_url(&group.expr),
        _ => false,
    }
}

fn expression_is_runtime_route(
    expression: &Expr,
    expected: registry::RuntimeExecutionRoute,
) -> bool {
    let Expr::Path(path) = expression else {
        return false;
    };
    let expected = format!("{expected:?}");
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == expected)
}

fn bind_http_matches_registered_route_and_post(
    bind: &ExprMethodCall,
    expected_route: Option<registry::RuntimeExecutionRoute>,
    post_builders: &HashSet<String>,
    aliases: &HashMap<String, Vec<String>>,
) -> bool {
    bind.args.len() == 2
        && expected_route.is_some_and(|route| expression_is_runtime_route(&bind.args[0], route))
        && expression_is_post_builder(&bind.args[1], post_builders, aliases)
}

fn path_is_ident(path: &ExprPath, ident: &str) -> bool {
    !ident.is_empty() && path.path.is_ident(ident)
}

fn receiver_looks_process(expression: &Expr, process_commands: &HashSet<String>) -> bool {
    match expression {
        Expr::Path(path) => path.path.segments.last().is_some_and(|segment| {
            let ident = segment.ident.to_string();
            process_commands.contains(&ident)
                || matches!(ident.as_str(), "cmd" | "command" | "child")
        }),
        Expr::MethodCall(call) => {
            call.method == "bind_command"
                || receiver_looks_process(&call.receiver, process_commands)
        }
        Expr::Call(call) => matches!(
            call.func.as_ref(),
            Expr::Path(path)
                if path.path.segments.iter().any(|segment| segment.ident == "Command")
        ),
        Expr::Try(tried) => receiver_looks_process(&tried.expr, process_commands),
        Expr::Paren(paren) => receiver_looks_process(&paren.expr, process_commands),
        _ => false,
    }
}

fn receiver_looks_http(expression: &Expr, http_clients: &HashSet<String>) -> bool {
    match expression {
        Expr::Path(path) => path.path.segments.last().is_some_and(|segment| {
            let ident = segment.ident.to_string();
            http_clients.contains(&ident)
                || matches!(
                    ident.as_str(),
                    "http" | "client" | "request" | "request_builder"
                )
        }),
        Expr::Field(field) => matches!(
            &field.member,
            syn::Member::Named(ident) if ident == "http" || ident == "client"
        ),
        Expr::Call(_) => expression_constructs_http_client(expression),
        Expr::MethodCall(call) => receiver_looks_http(&call.receiver, http_clients),
        Expr::Reference(reference) => receiver_looks_http(&reference.expr, http_clients),
        _ => false,
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
