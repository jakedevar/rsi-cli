//! Offline provider-capability completeness and drift validation.
//!
//! The validator consumes the daemon's canonical typed catalog projection and
//! field contracts. It does not maintain an independent provider checklist.

use crate::provider_capabilities::{
    CODEX_CATALOG_MODEL_FIELDS, CODEX_CATALOG_ROOT_FIELDS, CODEX_TRANSPORT_FALLBACKS,
    CatalogFieldContract, CatalogFieldHandling, CodexCatalogModel, CodexCatalogSnapshot,
    MAX_VALIDATED_CONTEXT_TOKENS, OFFICIAL_MODEL_CAPACITIES, REPOSITORY_MODEL_FALLBACKS,
    RETIRED_CLAUDE_MODEL_WINDOWS, VALIDATED_CODEX_CLI_VERSION, VALIDATED_CODEX_RAW_CATALOG_DIGEST,
    VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST, parse_codex_catalog_snapshot,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, FnArg, ImplItemFn, ItemFn, ItemMod, Lit, Pat, Signature, Stmt, Type};
use walkdir::WalkDir;

const SEMANTIC_FIXTURE_PATH: &str = "crates/rsid/tests/fixtures/codex-models-0.155.1.json";
const SCHEMA_FIXTURE_PATH: &str = "crates/rsid/tests/fixtures/codex-models-0.155.1-schema.json";
const ACCEPTANCE_PATH: &str =
    "thoughts/shared/verification/2026-09-03-codex-context-capability-acceptance.md";
const DOCUMENTATION_PATH: &str = "docs/provider-capabilities.md";
const REQUIRED_CONSUMER_ROLES_PATH: &str =
    "crates/rsid/provider-capability-required-consumer-roles.txt";
const REQUIRED_CONSUMER_ROLES: &str =
    include_str!("../provider-capability-required-consumer-roles.txt");
const CANONICAL_PROVIDER_CAPABILITY_MODULE: &str = "crates/rsid/src/provider_capabilities.rs";
const PROVIDER_CAPABILITY_VALIDATOR_MODULE: &str =
    "crates/rsid/src/provider_capability_validation.rs";
const GENERATED_START: &str = "<!-- BEGIN GENERATED: provider-capability-validator -->";
const GENERATED_END: &str = "<!-- END GENERATED: provider-capability-validator -->";

#[derive(Debug, Clone)]
pub struct ValidationInput {
    repo_root: PathBuf,
    semantic_fixture_path: PathBuf,
    schema_fixture_path: PathBuf,
    acceptance_path: PathBuf,
    documentation_path: PathBuf,
    expected_cli_version: String,
    expected_semantic_digest: String,
    expected_raw_digest: String,
    consumers: Vec<ConsumerContract>,
}

#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub inventory: Vec<String>,
}

impl ValidationInput {
    #[must_use]
    pub fn production(repo_root: PathBuf) -> Self {
        Self {
            semantic_fixture_path: repo_root.join(SEMANTIC_FIXTURE_PATH),
            schema_fixture_path: repo_root.join(SCHEMA_FIXTURE_PATH),
            acceptance_path: repo_root.join(ACCEPTANCE_PATH),
            documentation_path: repo_root.join(DOCUMENTATION_PATH),
            repo_root,
            expected_cli_version: VALIDATED_CODEX_CLI_VERSION.to_string(),
            expected_semantic_digest: VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST.to_string(),
            expected_raw_digest: VALIDATED_CODEX_RAW_CATALOG_DIGEST.to_string(),
            consumers: CONSUMER_INVENTORY.to_vec(),
        }
    }
}

pub fn validate_repository(input: ValidationInput) -> Result<ValidationReport, String> {
    let mut violations = Vec::new();
    validate_field_contracts(&mut violations);
    validate_provenance(&input, &mut violations);
    validate_mapping_uniqueness(
        "repository model fallback",
        REPOSITORY_MODEL_FALLBACKS,
        &mut violations,
    );
    validate_mapping_uniqueness(
        "Codex transport fallback",
        CODEX_TRANSPORT_FALLBACKS,
        &mut violations,
    );
    validate_mapping_uniqueness(
        "retired Claude model window",
        RETIRED_CLAUDE_MODEL_WINDOWS,
        &mut violations,
    );
    validate_official_records(None, &mut violations);
    validate_consumer_inventory(&input.repo_root, &input.consumers, &mut violations);
    scan_repository_sources(&input.repo_root, &mut violations);

    let snapshot = load_and_validate_catalog(&input, &mut violations);
    if let Some(snapshot) = snapshot.as_ref() {
        validate_official_records(Some(snapshot), &mut violations);
        validate_documentation(&input, snapshot, &mut violations);
    }

    if !violations.is_empty() {
        violations.sort();
        violations.dedup();
        return Err(format!(
            "provider-capability validation failed:\n{}",
            violations.join("\n")
        ));
    }

    let mut inventory = input
        .consumers
        .iter()
        .map(|consumer| {
            format!(
                "role={} path={} symbol={}",
                consumer.role.as_str(),
                consumer.path,
                consumer.symbol
            )
        })
        .collect::<Vec<_>>();
    inventory.push(format!(
        "catalog={} version={} semantic_digest={} raw_digest={}",
        SEMANTIC_FIXTURE_PATH,
        input.expected_cli_version,
        input.expected_semantic_digest,
        input.expected_raw_digest
    ));
    inventory.sort();
    Ok(ValidationReport { inventory })
}

pub fn render_provider_capability_documentation() -> Result<String, String> {
    let snapshot = parse_codex_catalog_snapshot(
        VALIDATED_CODEX_CLI_VERSION,
        include_bytes!("../tests/fixtures/codex-models-0.155.1.json"),
        fixture_observed_at(),
    )
    .map_err(|error| format!("failed to parse bundled semantic fixture: {error}"))?;
    Ok(render_documentation(&snapshot))
}

fn load_and_validate_catalog(
    input: &ValidationInput,
    violations: &mut Vec<String>,
) -> Option<CodexCatalogSnapshot> {
    let raw = match fs::read(&input.semantic_fixture_path) {
        Ok(raw) => raw,
        Err(error) => {
            violations.push(format!(
                "{}: failed to read semantic catalog fixture: {error}",
                relative(&input.repo_root, &input.semantic_fixture_path)
            ));
            return None;
        }
    };
    if raw.len() > 64 * 1024 {
        violations.push(format!(
            "{}: semantic fixture exceeds the 64 KiB reviewability bound",
            relative(&input.repo_root, &input.semantic_fixture_path)
        ));
    }
    let actual_digest = sha256_digest(&raw);
    if actual_digest != input.expected_semantic_digest {
        violations.push(format!(
            "{}: semantic fixture digest drift: expected {}, got {actual_digest}",
            relative(&input.repo_root, &input.semantic_fixture_path),
            input.expected_semantic_digest
        ));
    }

    let root = match parse_json_object(&raw, "semantic catalog fixture", violations) {
        Some(root) => root,
        None => return None,
    };
    validate_root_and_model_keys(
        &root,
        false,
        relative(&input.repo_root, &input.semantic_fixture_path),
        violations,
    );

    let snapshot = match parse_codex_catalog_snapshot(
        &input.expected_cli_version,
        &raw,
        fixture_observed_at(),
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            violations.push(format!(
                "{}: typed catalog parse failed: {error}",
                relative(&input.repo_root, &input.semantic_fixture_path)
            ));
            return None;
        }
    };
    if snapshot.key.cli_version != input.expected_cli_version {
        violations.push("typed catalog projection lost exact CLI-version provenance".to_string());
    }
    if snapshot.key.content_digest.as_str() != input.expected_semantic_digest {
        violations.push("typed catalog projection lost semantic-digest provenance".to_string());
    }
    validate_semantic_projection(&root, &snapshot, violations);

    match fs::read(&input.schema_fixture_path) {
        Ok(schema_raw) => {
            if schema_raw.len() > 16 * 1024 {
                violations.push(format!(
                    "{}: schema fixture exceeds the 16 KiB bound",
                    relative(&input.repo_root, &input.schema_fixture_path)
                ));
            }
            if let Some(schema_root) =
                parse_json_object(&schema_raw, "bounded schema fixture", violations)
            {
                validate_root_and_model_keys(
                    &schema_root,
                    true,
                    relative(&input.repo_root, &input.schema_fixture_path),
                    violations,
                );
            }
        }
        Err(error) => violations.push(format!(
            "{}: failed to read schema fixture: {error}",
            relative(&input.repo_root, &input.schema_fixture_path)
        )),
    }

    Some(snapshot)
}

fn parse_json_object(
    raw: &[u8],
    label: &str,
    violations: &mut Vec<String>,
) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(root)) => Some(root),
        Ok(_) => {
            violations.push(format!("{label} root must be an object"));
            None
        }
        Err(error) => {
            violations.push(format!("{label} is invalid JSON: {error}"));
            None
        }
    }
}

fn validate_field_contracts(violations: &mut Vec<String>) {
    validate_contract_set("catalog root", CODEX_CATALOG_ROOT_FIELDS, violations);
    validate_contract_set("catalog model", CODEX_CATALOG_MODEL_FIELDS, violations);
    if CODEX_CATALOG_MODEL_FIELDS.len() != 39 {
        violations.push(format!(
            "catalog model contract must classify the installed 39-key catalog, found {}",
            CODEX_CATALOG_MODEL_FIELDS.len()
        ));
    }
}

fn validate_contract_set(
    label: &str,
    contracts: &[CatalogFieldContract],
    violations: &mut Vec<String>,
) {
    let mut keys = HashSet::new();
    let mut prior = None;
    for contract in contracts {
        if !keys.insert(contract.key) {
            violations.push(format!(
                "duplicate {label} field contract `{}`",
                contract.key
            ));
        }
        if prior.is_some_and(|prior| prior >= contract.key) {
            violations.push(format!(
                "{label} field contracts must be strictly key-sorted at `{}`",
                contract.key
            ));
        }
        prior = Some(contract.key);
        if contract.category.trim().is_empty() {
            violations.push(format!(
                "{label} field `{}` has no semantic category",
                contract.key
            ));
        }
        let detail = match contract.handling {
            CatalogFieldHandling::Consumed(projection)
            | CatalogFieldHandling::Ignored(projection) => projection,
        };
        if detail.trim().len() < 8 {
            violations.push(format!(
                "{label} field `{}` lacks a specific handling explanation",
                contract.key
            ));
        }
    }
}

fn validate_root_and_model_keys(
    root: &serde_json::Map<String, Value>,
    _schema_fixture: bool,
    label: &str,
    violations: &mut Vec<String>,
) {
    let expected_root = contract_keys(CODEX_CATALOG_ROOT_FIELDS);
    validate_exact_keys(
        label,
        root.keys().map(String::as_str),
        &expected_root,
        violations,
    );
    let Some(models) = root.get("models").and_then(Value::as_array) else {
        violations.push(format!("{label}: `models` must be an array"));
        return;
    };
    if models.is_empty() {
        violations.push(format!("{label}: `models` must not be empty"));
        return;
    }
    let expected_models = contract_keys(CODEX_CATALOG_MODEL_FIELDS);
    for (index, model) in models.iter().enumerate() {
        let Some(model) = model.as_object() else {
            violations.push(format!("{label}: model {index} must be an object"));
            continue;
        };
        let expected = &expected_models;
        validate_exact_keys(
            &format!("{label}: model {index}"),
            model.keys().map(String::as_str),
            expected,
            violations,
        );
    }
}

fn validate_exact_keys<'a>(
    label: &str,
    actual: impl Iterator<Item = &'a str>,
    expected: &BTreeSet<&str>,
    violations: &mut Vec<String>,
) {
    let actual = actual.collect::<BTreeSet<_>>();
    for key in actual.difference(expected) {
        violations.push(format!("{label}: unknown catalog field `{key}`"));
    }
    for key in expected.difference(&actual) {
        violations.push(format!("{label}: missing classified catalog field `{key}`"));
    }
}

fn contract_keys(contracts: &[CatalogFieldContract]) -> BTreeSet<&str> {
    contracts.iter().map(|contract| contract.key).collect()
}

fn validate_semantic_projection(
    root: &serde_json::Map<String, Value>,
    snapshot: &CodexCatalogSnapshot,
    violations: &mut Vec<String>,
) {
    let Some(raw_models) = root.get("models").and_then(Value::as_array) else {
        return;
    };
    if raw_models.len() != snapshot.models.len() {
        violations.push("typed catalog projection discarded one or more models".to_string());
        return;
    }
    let projected = snapshot
        .models
        .iter()
        .map(|model| (model.slug.as_str(), model))
        .collect::<BTreeMap<_, _>>();
    if projected.len() != snapshot.models.len() {
        violations.push("typed catalog projection contains duplicate model IDs".to_string());
    }
    for raw in raw_models {
        let Some(raw) = raw.as_object() else {
            continue;
        };
        let Some(slug) = raw.get("slug").and_then(Value::as_str) else {
            violations.push("catalog model is missing a string slug".to_string());
            continue;
        };
        let Some(model) = projected.get(slug).copied() else {
            violations.push(format!("typed catalog projection discarded model `{slug}`"));
            continue;
        };
        compare_model_projection(raw, model, violations);
    }
}

fn compare_model_projection(
    raw: &serde_json::Map<String, Value>,
    model: &CodexCatalogModel,
    violations: &mut Vec<String>,
) {
    let slug = model.slug.as_str();
    compare_value(
        slug,
        "slug",
        raw.get("slug").and_then(Value::as_str),
        Some(model.slug.as_str()),
        violations,
    );
    compare_value(
        slug,
        "display_name",
        raw.get("display_name").and_then(Value::as_str),
        model.display_name.as_deref(),
        violations,
    );
    compare_value(
        slug,
        "description",
        raw.get("description").and_then(Value::as_str),
        model.description.as_deref(),
        violations,
    );
    compare_value(
        slug,
        "visibility",
        raw.get("visibility").and_then(Value::as_str),
        model.visibility.as_deref(),
        violations,
    );
    compare_value(
        slug,
        "priority",
        raw.get("priority").and_then(Value::as_i64),
        model.priority,
        violations,
    );
    compare_value(
        slug,
        "supported_in_api",
        raw.get("supported_in_api").and_then(Value::as_bool),
        model.supported_in_api,
        violations,
    );
    compare_value(
        slug,
        "context_window",
        raw.get("context_window").and_then(Value::as_u64),
        model.capacity.provider_default_tokens,
        violations,
    );
    compare_value(
        slug,
        "max_context_window",
        raw.get("max_context_window").and_then(Value::as_u64),
        model.capacity.provider_max_tokens,
        violations,
    );
    compare_value(
        slug,
        "effective_context_window_percent",
        raw.get("effective_context_window_percent")
            .and_then(Value::as_u64),
        model.capacity.effective_percent.map(u64::from),
        violations,
    );
    compare_value(
        slug,
        "default_reasoning_level",
        raw.get("default_reasoning_level").and_then(Value::as_str),
        model.default_reasoning_level.as_deref(),
        violations,
    );

    let raw_efforts = raw
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .map(|levels| {
            levels
                .iter()
                .map(|level| {
                    (
                        level.get("effort").and_then(Value::as_str),
                        level.get("description").and_then(Value::as_str),
                    )
                })
                .collect::<Vec<_>>()
        });
    let projected_efforts = model
        .supported_reasoning_levels
        .iter()
        .map(|level| (Some(level.effort.as_str()), level.description.as_deref()))
        .collect::<Vec<_>>();
    compare_value(
        slug,
        "supported_reasoning_levels",
        raw_efforts.as_deref(),
        Some(projected_efforts.as_slice()),
        violations,
    );

    let raw_modalities = raw
        .get("input_modalities")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>());
    compare_value(
        slug,
        "input_modalities",
        raw_modalities.as_deref(),
        Some(
            model
                .tools
                .input_modalities
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice(),
        ),
        violations,
    );
    let unique_modalities = model
        .tools
        .input_modalities
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<HashSet<_>>();
    if unique_modalities.len() != model.tools.input_modalities.len()
        || unique_modalities.contains("")
    {
        violations.push(format!(
            "catalog model `{slug}` has empty or duplicate input modalities"
        ));
    }
    compare_value(
        slug,
        "apply_patch_tool_type",
        raw.get("apply_patch_tool_type").and_then(Value::as_str),
        model.tools.apply_patch_tool_type.as_deref(),
        violations,
    );
    compare_value(
        slug,
        "experimental_supported_tools",
        raw.get("experimental_supported_tools")
            .and_then(Value::as_array)
            .map(Vec::as_slice),
        Some(model.tools.experimental_supported_tools.as_slice()),
        violations,
    );
    compare_value(
        slug,
        "shell_type",
        raw.get("shell_type").and_then(Value::as_str),
        model.tools.shell_type.as_deref(),
        violations,
    );
    compare_value(
        slug,
        "supports_image_detail_original",
        raw.get("supports_image_detail_original")
            .and_then(Value::as_bool),
        model.tools.supports_image_detail_original,
        violations,
    );
    compare_value(
        slug,
        "supports_search_tool",
        raw.get("supports_search_tool").and_then(Value::as_bool),
        model.tools.supports_search_tool,
        violations,
    );
    compare_value(
        slug,
        "tool_mode",
        raw.get("tool_mode").and_then(Value::as_str),
        model.tools.tool_mode.as_deref(),
        violations,
    );
    compare_value(
        slug,
        "web_search_tool_type",
        raw.get("web_search_tool_type").and_then(Value::as_str),
        model.tools.web_search_tool_type.as_deref(),
        violations,
    );
    compare_value(
        slug,
        "comp_hash",
        raw.get("comp_hash").and_then(Value::as_str),
        model.compaction.comp_hash.as_deref(),
        violations,
    );
    compare_value(
        slug,
        "truncation_policy",
        raw.get("truncation_policy"),
        model.compaction.truncation_policy.as_ref(),
        violations,
    );

    if model.capacity.max_output_tokens.is_some() {
        violations.push(format!(
            "catalog model `{slug}` incorrectly treats max output as installed-catalog evidence"
        ));
    }
    if model.capacity.compaction_limit_tokens.is_some() {
        violations.push(format!(
            "catalog model `{slug}` incorrectly treats truncation metadata as a compaction limit"
        ));
    }
}

fn compare_value<T: PartialEq + std::fmt::Debug>(
    slug: &str,
    field: &str,
    raw: Option<T>,
    projected: Option<T>,
    violations: &mut Vec<String>,
) {
    if raw != projected {
        violations.push(format!(
            "catalog model `{slug}` dropped or changed `{field}`: raw={raw:?} projected={projected:?}"
        ));
    }
}

fn validate_provenance(input: &ValidationInput, violations: &mut Vec<String>) {
    if input.expected_cli_version != VALIDATED_CODEX_CLI_VERSION
        || input.expected_cli_version.trim().is_empty()
    {
        violations.push(format!(
            "catalog provenance requires exact CLI version `{VALIDATED_CODEX_CLI_VERSION}`"
        ));
    }
    for (label, digest, expected) in [
        (
            "semantic fixture",
            input.expected_semantic_digest.as_str(),
            VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST,
        ),
        (
            "raw installed catalog",
            input.expected_raw_digest.as_str(),
            VALIDATED_CODEX_RAW_CATALOG_DIGEST,
        ),
    ] {
        if digest != expected || !is_sha256_digest(digest) {
            violations.push(format!(
                "{label} provenance requires exact SHA-256 `{expected}`"
            ));
        }
    }
    if input.expected_semantic_digest == input.expected_raw_digest {
        violations.push(
            "semantic fixture digest must remain distinct from raw installed provenance"
                .to_string(),
        );
    }
    match fs::read_to_string(&input.acceptance_path) {
        Ok(acceptance) => {
            if !acceptance.contains(&input.expected_cli_version) {
                violations.push(format!(
                    "{}: acceptance artifact lost exact CLI-version provenance",
                    relative(&input.repo_root, &input.acceptance_path)
                ));
            }
            let raw_hex = input
                .expected_raw_digest
                .strip_prefix("sha256:")
                .unwrap_or(&input.expected_raw_digest);
            if !acceptance.contains(raw_hex) {
                violations.push(format!(
                    "{}: acceptance artifact lost raw catalog digest provenance",
                    relative(&input.repo_root, &input.acceptance_path)
                ));
            }
        }
        Err(error) => violations.push(format!(
            "{}: failed to read acceptance provenance: {error}",
            relative(&input.repo_root, &input.acceptance_path)
        )),
    }
}

fn validate_official_records(
    snapshot: Option<&CodexCatalogSnapshot>,
    violations: &mut Vec<String>,
) {
    let mut slugs = HashSet::new();
    for record in OFFICIAL_MODEL_CAPACITIES {
        if !slugs.insert(record.slug.to_ascii_lowercase()) {
            violations.push(format!(
                "duplicate official capacity record `{}`",
                record.slug
            ));
        }
    }
    let astra = OFFICIAL_MODEL_CAPACITIES
        .iter()
        .find(|record| record.slug == "gpt-6-astra");
    match astra {
        Some(record)
            if record.advertised_max_tokens == 1_050_000
                && record.max_output_tokens == 128_000
                && record.documentation_url
                    == "https://developers.openai.com/api/docs/models/gpt-6-astra" => {}
        _ => violations.push(
            "official Astra record must remain descriptive 1,050,000 context / 128,000 output"
                .to_string(),
        ),
    }
    let Some(snapshot) = snapshot else {
        return;
    };
    let Some(astra) = snapshot
        .models
        .iter()
        .find(|model| model.slug == "gpt-6-astra")
    else {
        violations.push("semantic fixture is missing gpt-6-astra".to_string());
        return;
    };
    if astra.capacity.provider_default_tokens != Some(272_000)
        || astra.capacity.provider_max_tokens != Some(872_000)
        || astra.capacity.effective_percent != Some(95)
        || astra.capacity.provider_effective_default_tokens() != Some(258_400)
    {
        violations.push("installed Astra capacity semantics drifted from 0.155.1".to_string());
    }
    if astra.capacity.provider_effective_default_tokens() == Some(1_050_000)
        || astra.capacity.provider_effective_default_tokens() == Some(128_000)
    {
        violations.push("official Astra descriptions became active CLI capacity".to_string());
    }
}

fn validate_mapping_uniqueness(
    label: &str,
    mappings: &[(&str, u64)],
    violations: &mut Vec<String>,
) {
    let mut patterns = HashSet::new();
    for (pattern, capacity) in mappings {
        let normalized = pattern.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            violations.push(format!("{label} contains an empty model pattern"));
        }
        if !patterns.insert(normalized) {
            violations.push(format!("{label} duplicates model pattern `{pattern}`"));
        }
        if *capacity == 0 || *capacity > MAX_VALIDATED_CONTEXT_TOKENS {
            violations.push(format!(
                "{label} pattern `{pattern}` has invalid capacity {capacity}"
            ));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ConsumerRole {
    StartupCatalogRefresh,
    Launch,
    RestorationReopen,
    LiveMonitor,
    MemoryFlush,
    Rotation,
    ContextInjection,
    HarnessFullWindowCompaction,
    Persistence,
    RpcSession,
    BusPublication,
    Polling,
    F3Detail,
    DetailHeaderContext,
    WideInspectorDetail,
    WideInspectorCompact,
    SessionListCompactRow,
    Tests,
}

impl ConsumerRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::StartupCatalogRefresh => "startup-catalog-refresh",
            Self::Launch => "launch",
            Self::RestorationReopen => "restoration-reopen",
            Self::LiveMonitor => "live-monitor",
            Self::MemoryFlush => "memory-flush",
            Self::Rotation => "rotation",
            Self::ContextInjection => "context-injection",
            Self::HarnessFullWindowCompaction => "harness-full-window-compaction",
            Self::Persistence => "persistence",
            Self::RpcSession => "rpc-session",
            Self::BusPublication => "bus-publication",
            Self::Polling => "polling",
            Self::F3Detail => "f3-detail",
            Self::DetailHeaderContext => "detail-header-context",
            Self::WideInspectorDetail => "wide-inspector-detail",
            Self::WideInspectorCompact => "wide-inspector-compact",
            Self::SessionListCompactRow => "session-list-compact-row",
            Self::Tests => "tests",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FlowSource {
    Call(&'static str),
    Parameter(&'static str),
    FieldPath(&'static str),
}

impl FlowSource {
    fn describe(self) -> String {
        match self {
            Self::Call(name) => format!("call:{name}"),
            Self::Parameter(name) => format!("parameter:{name}"),
            Self::FieldPath(path) => format!("field:{path}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FlowSink {
    Call(&'static str),
    StructField(&'static str),
    Assignment(&'static str),
    Macro(&'static str),
    Return,
}

impl FlowSink {
    fn describe(self) -> String {
        match self {
            Self::Call(name) => format!("call:{name}"),
            Self::StructField(name) => format!("field:{name}"),
            Self::Assignment(path) => format!("assignment:{path}"),
            Self::Macro(name) => format!("macro:{name}"),
            Self::Return => "return".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ConsumerContract {
    role: ConsumerRole,
    path: &'static str,
    /// Free function name or `Type::method` for an inherent method.
    symbol: &'static str,
    source: FlowSource,
    sinks: &'static [FlowSink],
}

const CONSUMER_INVENTORY: &[ConsumerContract] = &[
    ConsumerContract {
        role: ConsumerRole::StartupCatalogRefresh,
        path: "crates/rsid/src/session/mod.rs",
        symbol: "SessionManager::refresh_codex_catalog_at_startup",
        source: FlowSource::Call("provider_capabilities"),
        sinks: &[FlowSink::Call("refresh_catalog")],
    },
    ConsumerContract {
        role: ConsumerRole::Launch,
        path: "crates/rsid/src/session/launch.rs",
        symbol: "build_starting_session",
        source: FlowSource::Call("resolve_fresh_context_budget"),
        sinks: &[
            FlowSink::StructField("context_window"),
            FlowSink::StructField("resolved_context_budget"),
        ],
    },
    ConsumerContract {
        role: ConsumerRole::RestorationReopen,
        path: "crates/rsid/src/session/lifecycle.rs",
        symbol: "SessionManager::continue_session_with_delivery",
        source: FlowSource::Call("resolve_new_incarnation_context_budget"),
        sinks: &[
            FlowSink::Call("compare_and_update_session_model"),
            FlowSink::Call("install_context_budget"),
        ],
    },
    ConsumerContract {
        role: ConsumerRole::LiveMonitor,
        path: "crates/rsid/src/session/monitor.rs",
        symbol: "persist_runtime_context_observation",
        source: FlowSource::Call("resolve_runtime_context_budget"),
        sinks: &[
            FlowSink::Call("compare_and_update_session_model"),
            FlowSink::Call("install_context_budget"),
            FlowSink::Return,
        ],
    },
    ConsumerContract {
        role: ConsumerRole::MemoryFlush,
        path: "crates/rsid/src/session/monitor.rs",
        symbol: "memory_flush_turn_candidate",
        source: FlowSource::Call("context_budget"),
        sinks: &[
            FlowSink::Call("should_run_memory_flush"),
            FlowSink::StructField("active_tokens"),
        ],
    },
    ConsumerContract {
        role: ConsumerRole::Rotation,
        path: "crates/rsid/src/session/rotation.rs",
        symbol: "SessionManager::rotate_completed_session",
        source: FlowSource::Call("resolve_new_incarnation_context_budget"),
        sinks: &[
            FlowSink::StructField("context_window"),
            FlowSink::StructField("resolved_context_budget"),
        ],
    },
    ConsumerContract {
        role: ConsumerRole::ContextInjection,
        path: "crates/rsid/src/session/launch.rs",
        symbol: "SessionManager::launch_session_with_retry_admission",
        source: FlowSource::Call("context_injection_allowance"),
        sinks: &[FlowSink::Call("assemble")],
    },
    ConsumerContract {
        role: ConsumerRole::HarnessFullWindowCompaction,
        path: "crates/rsid/src/session/harness/mod.rs",
        symbol: "HarnessClient::launch",
        source: FlowSource::Parameter("resolved_context_budget"),
        sinks: &[FlowSink::Call("run_harness_loop")],
    },
    ConsumerContract {
        role: ConsumerRole::Persistence,
        path: "crates/rsid/src/store/sessions.rs",
        symbol: "persisted_context_budget",
        source: FlowSource::Parameter("resolved"),
        sinks: &[
            FlowSink::StructField("source"),
            FlowSink::StructField("source_version"),
            FlowSink::StructField("source_digest"),
            FlowSink::StructField("observed_at"),
        ],
    },
    ConsumerContract {
        role: ConsumerRole::RpcSession,
        path: "crates/rsid/src/session/queries.rs",
        symbol: "rehydrate_context_budget_projection",
        source: FlowSource::Call("rehydrate_resolved_context_budget"),
        sinks: &[FlowSink::Assignment("session.resolved_context_budget")],
    },
    ConsumerContract {
        role: ConsumerRole::BusPublication,
        path: "crates/rsid/src/monitor.rs",
        symbol: "publish_context_usage",
        source: FlowSource::Parameter("resolved_context_budget"),
        sinks: &[
            FlowSink::StructField("context_window"),
            FlowSink::StructField("resolved_context_budget"),
        ],
    },
    ConsumerContract {
        role: ConsumerRole::Polling,
        path: "crates/rsi/src/app/polling.rs",
        symbol: "App::apply_push_event",
        source: FlowSource::FieldPath("parsed.resolved_context_budget"),
        sinks: &[FlowSink::Assignment(
            "state.session.resolved_context_budget",
        )],
    },
    ConsumerContract {
        role: ConsumerRole::F3Detail,
        path: "crates/rsi/src/ui/overlay/session_info.rs",
        symbol: "session_info_lines",
        source: FlowSource::Call("detail_rows"),
        sinks: &[FlowSink::Call("field_line")],
    },
    ConsumerContract {
        role: ConsumerRole::WideInspectorDetail,
        path: "crates/rsi/src/ui/session.rs",
        symbol: "render_inspector_context",
        source: FlowSource::Call("detail_rows"),
        sinks: &[FlowSink::Call("push_inspector_section")],
    },
    ConsumerContract {
        role: ConsumerRole::DetailHeaderContext,
        path: "crates/rsi/src/ui/status.rs",
        symbol: "render_context_percent_segment_for",
        source: FlowSource::Call("compact_label"),
        sinks: &[FlowSink::Call("styled")],
    },
    ConsumerContract {
        role: ConsumerRole::WideInspectorCompact,
        path: "crates/rsi/src/ui/session.rs",
        symbol: "render_inspector_runtime",
        source: FlowSource::FieldPath("runtime.context"),
        sinks: &[FlowSink::Call("compact_label")],
    },
    ConsumerContract {
        role: ConsumerRole::SessionListCompactRow,
        path: "crates/rsi/src/types/row.rs",
        symbol: "compute_session_row_for_state_with_focus",
        source: FlowSource::Call("compute_context_budget_view"),
        sinks: &[FlowSink::Call("compact_label")],
    },
    ConsumerContract {
        role: ConsumerRole::Tests,
        path: "crates/rsid/src/provider_capabilities.rs",
        symbol: "real_codex_0_155_1_fixture_preserves_capacity_reasoning_and_projection",
        source: FlowSource::Call("fixture_snapshot"),
        sinks: &[FlowSink::Macro("assert_eq")],
    },
];

fn validate_consumer_inventory(
    repo_root: &Path,
    consumers: &[ConsumerContract],
    violations: &mut Vec<String>,
) {
    let required_roles = required_consumer_roles(violations);
    let mut role_counts = BTreeMap::<&str, usize>::new();
    for consumer in consumers {
        *role_counts.entry(consumer.role.as_str()).or_default() += 1;
    }
    for role in &required_roles {
        match role_counts.get(role.as_str()).copied().unwrap_or_default() {
            1 => {}
            0 => violations.push(format!(
                "consumer inventory is missing independently required role `{role}`"
            )),
            count => violations.push(format!(
                "consumer inventory repeats independently required role `{role}` {count} times"
            )),
        }
    }
    for role in role_counts.keys() {
        if !required_roles.contains(*role) {
            violations.push(format!("consumer inventory has unanchored role `{role}`"));
        }
    }

    for consumer in consumers {
        let path = repo_root.join(consumer.path);
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                violations.push(format!(
                    "{}: failed to read consumer `{}`: {error}",
                    consumer.path,
                    consumer.role.as_str()
                ));
                continue;
            }
        };
        let file = match syn::parse_file(&source) {
            Ok(file) => file,
            Err(error) => {
                violations.push(format!(
                    "{}: failed to parse consumer source: {error}",
                    consumer.path
                ));
                continue;
            }
        };
        let scopes = consumer_scopes(&file, consumer.symbol);
        if scopes.is_empty() {
            violations.push(format!(
                "{}: consumer role `{}` lost symbol `{}`",
                consumer.path,
                consumer.role.as_str(),
                consumer.symbol
            ));
            continue;
        }
        let mut best_reached = vec![false; consumer.sinks.len()];
        let mut source_present = false;
        let mut complete_scope = false;
        for scope in scopes {
            let analysis = analyze_consumer_flow(scope, *consumer);
            source_present |= analysis.source_present;
            complete_scope |=
                analysis.source_present && analysis.reached_sinks.iter().all(|reached| *reached);
            for (best, reached) in best_reached.iter_mut().zip(analysis.reached_sinks) {
                *best |= reached;
            }
        }
        if complete_scope {
            continue;
        }
        if !source_present {
            violations.push(format!(
                "{}: consumer role `{}` lost structural source `{}` inside symbol `{}`",
                consumer.path,
                consumer.role.as_str(),
                consumer.source.describe(),
                consumer.symbol
            ));
        }
        for (sink, reached) in consumer.sinks.iter().zip(best_reached.iter().copied()) {
            if !reached {
                violations.push(format!(
                    "{}: consumer role `{}` no longer carries `{}` into `{}` inside symbol `{}`",
                    consumer.path,
                    consumer.role.as_str(),
                    consumer.source.describe(),
                    sink.describe(),
                    consumer.symbol
                ));
            }
        }
        if source_present && best_reached.iter().all(|reached| *reached) {
            violations.push(format!(
                "{}: consumer role `{}` splits required dataflow across duplicate symbol `{}` definitions",
                consumer.path,
                consumer.role.as_str(),
                consumer.symbol
            ));
        }
    }
}

fn required_consumer_roles(violations: &mut Vec<String>) -> BTreeSet<String> {
    let mut roles = BTreeSet::new();
    for (line_index, raw) in REQUIRED_CONSUMER_ROLES.lines().enumerate() {
        let role = raw.trim();
        if role.is_empty() || role.starts_with('#') {
            continue;
        }
        if !role
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            violations.push(format!(
                "{REQUIRED_CONSUMER_ROLES_PATH}:{}: invalid required consumer role `{role}`",
                line_index + 1
            ));
            continue;
        }
        if !roles.insert(role.to_string()) {
            violations.push(format!(
                "{REQUIRED_CONSUMER_ROLES_PATH}:{}: duplicate required consumer role `{role}`",
                line_index + 1
            ));
        }
    }
    if roles.is_empty() {
        violations.push(format!(
            "{REQUIRED_CONSUMER_ROLES_PATH}: required consumer-role anchor is empty"
        ));
    }
    roles
}

#[derive(Clone, Copy)]
struct ConsumerScope<'ast> {
    signature: &'ast Signature,
    block: &'ast syn::Block,
}

fn consumer_scopes<'ast>(file: &'ast syn::File, symbol: &str) -> Vec<ConsumerScope<'ast>> {
    let (expected_owner, expected_name) = symbol
        .split_once("::")
        .map_or((None, symbol), |(owner, name)| (Some(owner), name));
    let mut scopes = Vec::new();
    collect_consumer_scopes(&file.items, expected_owner, expected_name, &mut scopes);
    scopes
}

fn collect_consumer_scopes<'ast>(
    items: &'ast [syn::Item],
    expected_owner: Option<&str>,
    expected_name: &str,
    scopes: &mut Vec<ConsumerScope<'ast>>,
) {
    for item in items {
        match item {
            syn::Item::Fn(function)
                if expected_owner.is_none() && function.sig.ident == expected_name =>
            {
                scopes.push(ConsumerScope {
                    signature: &function.sig,
                    block: &function.block,
                });
            }
            syn::Item::Impl(item_impl)
                if expected_owner.is_some_and(|owner| {
                    type_terminal_ident(item_impl.self_ty.as_ref()).as_deref() == Some(owner)
                }) =>
            {
                for impl_item in &item_impl.items {
                    if let syn::ImplItem::Fn(method) = impl_item
                        && method.sig.ident == expected_name
                    {
                        scopes.push(ConsumerScope {
                            signature: &method.sig,
                            block: &method.block,
                        });
                    }
                }
            }
            syn::Item::Mod(item_mod) => {
                if let Some((_, nested)) = &item_mod.content {
                    collect_consumer_scopes(nested, expected_owner, expected_name, scopes);
                }
            }
            _ => {}
        }
    }
}

fn type_terminal_ident(ty: &Type) -> Option<String> {
    let Type::Path(path) = ty else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

struct ConsumerFlowAnalysis {
    source_present: bool,
    reached_sinks: Vec<bool>,
}

fn analyze_consumer_flow(
    scope: ConsumerScope<'_>,
    contract: ConsumerContract,
) -> ConsumerFlowAnalysis {
    let mut tainted = HashSet::new();
    if let FlowSource::Parameter(expected) = contract.source {
        for input in &scope.signature.inputs {
            if let FnArg::Typed(argument) = input {
                let mut bindings = Vec::new();
                collect_pattern_bindings(&argument.pat, &mut bindings);
                if bindings.iter().any(|binding| binding == expected) {
                    tainted.insert(expected.to_string());
                }
            }
        }
    }

    loop {
        let mut collector = DerivedBindingCollector {
            source: contract.source,
            tainted: &tainted,
            derived: HashSet::new(),
        };
        collector.visit_block(scope.block);
        let previous_len = tainted.len();
        tainted.extend(collector.derived);
        if tainted.len() == previous_len {
            break;
        }
    }

    let source_present = match contract.source {
        FlowSource::Parameter(name) => tainted.contains(name),
        source => block_contains_dependency(scope.block, source, &HashSet::new()),
    };
    let mut collector = SinkFlowCollector {
        source: contract.source,
        tainted: &tainted,
        sinks: contract.sinks,
        reached: vec![false; contract.sinks.len()],
    };
    collector.visit_block(scope.block);
    if let Some(Stmt::Expr(expression, None)) = scope.block.stmts.last()
        && expression_contains_dependency(expression, contract.source, &tainted)
    {
        collector.mark_return();
    }
    ConsumerFlowAnalysis {
        source_present,
        reached_sinks: collector.reached,
    }
}

struct DerivedBindingCollector<'a> {
    source: FlowSource,
    tainted: &'a HashSet<String>,
    derived: HashSet<String>,
}

impl DerivedBindingCollector<'_> {
    fn derive_from(&mut self, pattern: &Pat, expression: &Expr) {
        if expression_contains_dependency(expression, self.source, self.tainted) {
            let mut bindings = Vec::new();
            collect_pattern_bindings(pattern, &mut bindings);
            self.derived.extend(bindings);
        }
    }
}

impl<'ast> Visit<'ast> for DerivedBindingCollector<'_> {
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(initializer) = &local.init {
            self.derive_from(&local.pat, &initializer.expr);
        }
        visit::visit_local(self, local);
    }

    fn visit_expr_for_loop(&mut self, expression: &'ast syn::ExprForLoop) {
        self.derive_from(&expression.pat, &expression.expr);
        visit::visit_expr_for_loop(self, expression);
    }

    fn visit_expr_let(&mut self, expression: &'ast syn::ExprLet) {
        self.derive_from(&expression.pat, &expression.expr);
        visit::visit_expr_let(self, expression);
    }

    fn visit_expr_match(&mut self, expression: &'ast syn::ExprMatch) {
        if expression_contains_dependency(&expression.expr, self.source, self.tainted) {
            for arm in &expression.arms {
                let mut bindings = Vec::new();
                collect_pattern_bindings(&arm.pat, &mut bindings);
                self.derived.extend(bindings);
            }
        }
        visit::visit_expr_match(self, expression);
    }

    fn visit_expr_assign(&mut self, expression: &'ast syn::ExprAssign) {
        if expression_contains_dependency(&expression.right, self.source, self.tainted)
            && let Expr::Path(path) = expression.left.as_ref()
            && path.path.segments.len() == 1
            && let Some(segment) = path.path.segments.first()
        {
            self.derived.insert(segment.ident.to_string());
        }
        visit::visit_expr_assign(self, expression);
    }
}

fn collect_pattern_bindings(pattern: &Pat, bindings: &mut Vec<String>) {
    struct Collector<'a>(&'a mut Vec<String>);
    impl<'ast> Visit<'ast> for Collector<'_> {
        fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
            self.0.push(pattern.ident.to_string());
            visit::visit_pat_ident(self, pattern);
        }
    }
    Collector(bindings).visit_pat(pattern);
}

fn block_contains_dependency(
    block: &syn::Block,
    source: FlowSource,
    tainted: &HashSet<String>,
) -> bool {
    let mut collector = DependencyCollector {
        source,
        tainted,
        found: false,
    };
    collector.visit_block(block);
    collector.found
}

fn expression_contains_dependency(
    expression: &Expr,
    source: FlowSource,
    tainted: &HashSet<String>,
) -> bool {
    let mut collector = DependencyCollector {
        source,
        tainted,
        found: false,
    };
    collector.visit_expr(expression);
    collector.found
}

struct DependencyCollector<'a> {
    source: FlowSource,
    tainted: &'a HashSet<String>,
    found: bool,
}

impl<'ast> Visit<'ast> for DependencyCollector<'_> {
    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if expression.path.segments.len() == 1
            && expression
                .path
                .segments
                .first()
                .is_some_and(|segment| self.tainted.contains(&segment.ident.to_string()))
        {
            self.found = true;
        }
        visit::visit_expr_path(self, expression);
    }

    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if let FlowSource::Call(expected) = self.source
            && call_name(&expression.func).as_deref() == Some(expected)
        {
            self.found = true;
        }
        visit::visit_expr_call(self, expression);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        if let FlowSource::Call(expected) = self.source
            && expression.method == expected
        {
            self.found = true;
        }
        visit::visit_expr_method_call(self, expression);
    }

    fn visit_expr_field(&mut self, expression: &'ast syn::ExprField) {
        if let FlowSource::FieldPath(expected) = self.source
            && field_expression_path(expression).is_some_and(|path| path == expected)
        {
            self.found = true;
        }
        visit::visit_expr_field(self, expression);
    }

    fn visit_macro(&mut self, expression: &'ast syn::Macro) {
        if macro_mentions_tainted(expression, self.tainted) {
            self.found = true;
        }
        visit::visit_macro(self, expression);
    }
}

struct SinkFlowCollector<'a> {
    source: FlowSource,
    tainted: &'a HashSet<String>,
    sinks: &'a [FlowSink],
    reached: Vec<bool>,
}

impl SinkFlowCollector<'_> {
    fn dependency(&self, expression: &Expr) -> bool {
        expression_contains_dependency(expression, self.source, self.tainted)
    }

    fn mark_matching(&mut self, mut matches: impl FnMut(FlowSink) -> bool) {
        for (index, sink) in self.sinks.iter().copied().enumerate() {
            if matches(sink) {
                self.reached[index] = true;
            }
        }
    }

    fn mark_return(&mut self) {
        self.mark_matching(|sink| matches!(sink, FlowSink::Return));
    }
}

impl<'ast> Visit<'ast> for SinkFlowCollector<'_> {
    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        let carries_dependency = expression.args.iter().any(|arg| self.dependency(arg));
        if carries_dependency {
            let name = call_name(&expression.func);
            self.mark_matching(|sink| matches!(sink, FlowSink::Call(expected) if name.as_deref() == Some(expected)));
        }
        visit::visit_expr_call(self, expression);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        let carries_dependency = self.dependency(&expression.receiver)
            || expression.args.iter().any(|arg| self.dependency(arg));
        if carries_dependency {
            let name = expression.method.to_string();
            self.mark_matching(|sink| matches!(sink, FlowSink::Call(expected) if name == expected));
        }
        visit::visit_expr_method_call(self, expression);
    }

    fn visit_expr_struct(&mut self, expression: &'ast syn::ExprStruct) {
        for field in &expression.fields {
            if self.dependency(&field.expr) {
                let member = member_name(&field.member);
                self.mark_matching(
                    |sink| matches!(sink, FlowSink::StructField(expected) if member == expected),
                );
            }
        }
        visit::visit_expr_struct(self, expression);
    }

    fn visit_expr_assign(&mut self, expression: &'ast syn::ExprAssign) {
        if self.dependency(&expression.right) {
            let target = expression_path(&expression.left);
            self.mark_matching(
                |sink| matches!(sink, FlowSink::Assignment(expected) if target.as_deref() == Some(expected)),
            );
        }
        visit::visit_expr_assign(self, expression);
    }

    fn visit_expr_return(&mut self, expression: &'ast syn::ExprReturn) {
        if expression
            .expr
            .as_deref()
            .is_some_and(|value| self.dependency(value))
        {
            self.mark_return();
        }
        visit::visit_expr_return(self, expression);
    }

    fn visit_macro(&mut self, expression: &'ast syn::Macro) {
        if macro_mentions_tainted(expression, self.tainted) {
            let name = expression
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string());
            self.mark_matching(
                |sink| matches!(sink, FlowSink::Macro(expected) if name.as_deref() == Some(expected)),
            );
        }
        visit::visit_macro(self, expression);
    }
}

fn call_name(expression: &Expr) -> Option<String> {
    let Expr::Path(path) = expression else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn expression_path(expression: &Expr) -> Option<String> {
    match expression {
        Expr::Path(path) => Some(
            path.path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        ),
        Expr::Field(field) => Some(format!(
            "{}.{}",
            expression_path(&field.base)?,
            member_name(&field.member)
        )),
        Expr::Paren(paren) => expression_path(&paren.expr),
        Expr::Reference(reference) => expression_path(&reference.expr),
        _ => None,
    }
}

fn field_expression_path(expression: &syn::ExprField) -> Option<String> {
    Some(format!(
        "{}.{}",
        expression_path(&expression.base)?,
        member_name(&expression.member)
    ))
}

fn member_name(member: &syn::Member) -> String {
    match member {
        syn::Member::Named(ident) => ident.to_string(),
        syn::Member::Unnamed(index) => index.index.to_string(),
    }
}

fn macro_mentions_tainted(expression: &syn::Macro, tainted: &HashSet<String>) -> bool {
    fn visit_tokens(tokens: proc_macro2::TokenStream, tainted: &HashSet<String>) -> bool {
        tokens.into_iter().any(|token| match token {
            proc_macro2::TokenTree::Ident(ident) => tainted.contains(&ident.to_string()),
            proc_macro2::TokenTree::Group(group) => visit_tokens(group.stream(), tainted),
            _ => false,
        })
    }

    if visit_tokens(expression.tokens.clone(), tainted) {
        return true;
    }
    let name = expression
        .path
        .segments
        .last()
        .map(|segment| segment.ident.to_string());
    if !matches!(
        name.as_deref(),
        Some("format" | "format_args" | "write" | "writeln")
    ) {
        return false;
    }
    let Some(proc_macro2::TokenTree::Literal(literal)) =
        expression.tokens.clone().into_iter().next()
    else {
        return false;
    };
    let Ok(format_literal) = syn::parse_str::<syn::LitStr>(&literal.to_string()) else {
        return false;
    };
    format_capture_names(&format_literal.value())
        .into_iter()
        .any(|capture| tainted.contains(&capture))
}

fn format_capture_names(format: &str) -> Vec<String> {
    let mut captures = Vec::new();
    let bytes = format.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'{' {
            index += 1;
            continue;
        }
        if bytes.get(index + 1) == Some(&b'{') {
            index += 2;
            continue;
        }
        let start = index + 1;
        let Some(relative_end) = bytes[start..].iter().position(|byte| *byte == b'}') else {
            break;
        };
        let field = &format[start..start + relative_end];
        let capture = field.split([':', '!']).next().unwrap_or_default().trim();
        if !capture.is_empty()
            && capture
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && capture
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            captures.push(capture.to_string());
        }
        index = start + relative_end + 1;
    }
    captures
}

fn scan_repository_sources(repo_root: &Path, violations: &mut Vec<String>) {
    let crates = repo_root.join("crates");
    for entry in WalkDir::new(&crates).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !entry.file_type().is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("rs")
        {
            continue;
        }
        let relative_path = relative(repo_root, path);
        // The basename is not authority: exactly this canonical daemon module
        // owns provider/model numeric mappings. The validator implementation
        // is separately excluded because it necessarily asserts known values.
        if relative_path == CANONICAL_PROVIDER_CAPABILITY_MODULE
            || relative_path == PROVIDER_CAPABILITY_VALIDATOR_MODULE
            || relative_path.contains("/tests/")
            || relative_path.contains("/fixtures/")
        {
            continue;
        }
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                violations.push(format!("{relative_path}: source scan read failed: {error}"));
                continue;
            }
        };
        let file = match syn::parse_file(&source) {
            Ok(file) => file,
            Err(error) => {
                violations.push(format!(
                    "{relative_path}: source scan parse failed: {error}"
                ));
                continue;
            }
        };
        let symbol_facts = build_symbol_facts(&file);
        let mut visitor = CapabilityDriftVisitor {
            path: &relative_path,
            violations,
            symbol_facts: &symbol_facts,
        };
        visitor.visit_file(&file);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LiteralFacts {
    model_ids: BTreeSet<String>,
    capacity_literals: BTreeSet<u64>,
}

impl LiteralFacts {
    fn merge(&mut self, other: Self) {
        self.model_ids.extend(other.model_ids);
        self.capacity_literals.extend(other.capacity_literals);
    }
}

enum ValueDefinition<'ast> {
    Expression(&'ast Expr),
    Block(&'ast syn::Block),
}

#[derive(Default)]
struct DefinitionCollector<'ast> {
    definitions: Vec<(String, ValueDefinition<'ast>)>,
}

impl<'ast> Visit<'ast> for DefinitionCollector<'ast> {
    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        if !has_test_cfg(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if !has_test_cfg(&item.attrs) {
            self.definitions.push((
                item.sig.ident.to_string(),
                ValueDefinition::Block(&item.block),
            ));
            visit::visit_item_fn(self, item);
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if !has_test_cfg(&item.attrs) {
            self.definitions.push((
                item.sig.ident.to_string(),
                ValueDefinition::Block(&item.block),
            ));
            visit::visit_impl_item_fn(self, item);
        }
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if !has_test_cfg(&item.attrs) {
            self.definitions.push((
                item.ident.to_string(),
                ValueDefinition::Expression(&item.expr),
            ));
            visit::visit_item_const(self, item);
        }
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        if !has_test_cfg(&item.attrs) {
            self.definitions.push((
                item.ident.to_string(),
                ValueDefinition::Expression(&item.expr),
            ));
            visit::visit_item_static(self, item);
        }
    }
}

fn build_symbol_facts(file: &syn::File) -> BTreeMap<String, LiteralFacts> {
    let mut definitions = DefinitionCollector::default();
    definitions.visit_file(file);
    let mut facts = BTreeMap::<String, LiteralFacts>::new();
    for _ in 0..=definitions.definitions.len() {
        let previous = facts.clone();
        for (name, definition) in &definitions.definitions {
            let mut collector = LiteralCollector::new(&previous);
            match definition {
                ValueDefinition::Expression(expression) => collector.visit_expr(expression),
                ValueDefinition::Block(block) => collector.visit_block(block),
            }
            facts
                .entry(name.clone())
                .or_default()
                .merge(collector.facts);
        }
        if facts == previous {
            break;
        }
    }
    facts
}

struct CapabilityDriftVisitor<'a, 'facts> {
    path: &'a str,
    violations: &'a mut Vec<String>,
    symbol_facts: &'facts BTreeMap<String, LiteralFacts>,
}

impl CapabilityDriftVisitor<'_, '_> {
    fn collect(&self, collect: impl FnOnce(&mut LiteralCollector<'_>)) -> LiteralFacts {
        let mut literals = LiteralCollector::new(self.symbol_facts);
        collect(&mut literals);
        literals.facts
    }

    fn collect_direct(&self, collect: impl FnOnce(&mut LiteralCollector<'_>)) -> LiteralFacts {
        let symbols = BTreeMap::new();
        let mut literals = LiteralCollector::new(&symbols);
        collect(&mut literals);
        literals.facts
    }

    fn check_facts(&mut self, kind: &str, facts: LiteralFacts) {
        if let (Some(model), Some(capacity)) =
            (facts.model_ids.first(), facts.capacity_literals.first())
        {
            self.violations.push(format!(
                "{}: {kind} contains model-to-numeric-context mapping `{model}` => {capacity}; register it in provider_capabilities.rs",
                self.path
            ));
        }
    }

    fn check_obsolete_ident(&mut self, ident: &str) {
        let normalized = ident.to_ascii_lowercase();
        if normalized.contains("codex")
            && normalized.contains("context_window")
            && (normalized.contains("for_model") || normalized.contains("resolve"))
        {
            self.violations.push(format!(
                "{}: obsolete provider-specific resolver `{ident}` must use the canonical registry",
                self.path
            ));
        }
    }
}

impl<'ast> Visit<'ast> for CapabilityDriftVisitor<'_, '_> {
    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        if has_test_cfg(&item.attrs) {
            return;
        }
        visit::visit_item_mod(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if has_test_cfg(&item.attrs) {
            return;
        }
        self.check_obsolete_ident(&item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if has_test_cfg(&item.attrs) {
            return;
        }
        self.check_obsolete_ident(&item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if !has_test_cfg(&item.attrs) {
            visit::visit_item_const(self, item);
        }
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        if !has_test_cfg(&item.attrs) {
            visit::visit_item_static(self, item);
        }
    }

    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if let Expr::Path(path) = expression.func.as_ref()
            && let Some(segment) = path.path.segments.last()
        {
            self.check_obsolete_ident(&segment.ident.to_string());
        }
        visit::visit_expr_call(self, expression);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        if expression.method == "insert" {
            let facts = self.collect(|collector| {
                for argument in &expression.args {
                    collector.visit_expr(argument);
                }
            });
            self.check_facts("map insertion", facts);
        }
        visit::visit_expr_method_call(self, expression);
    }

    fn visit_expr_array(&mut self, expression: &'ast syn::ExprArray) {
        let facts = self.collect(|collector| collector.visit_expr_array(expression));
        self.check_facts("array", facts);
        visit::visit_expr_array(self, expression);
    }

    fn visit_expr_tuple(&mut self, expression: &'ast syn::ExprTuple) {
        let facts = self.collect(|collector| collector.visit_expr_tuple(expression));
        self.check_facts("tuple", facts);
        visit::visit_expr_tuple(self, expression);
    }

    fn visit_expr_match(&mut self, expression: &'ast syn::ExprMatch) {
        for arm in &expression.arms {
            let facts = self.collect(|collector| collector.visit_arm(arm));
            self.check_facts("match arm", facts);
        }
        visit::visit_expr_match(self, expression);
    }

    fn visit_expr_if(&mut self, expression: &'ast syn::ExprIf) {
        // Conditionals remain a literal-local defense. Expanding every helper
        // called by a large branch conflates unrelated model defaults with
        // timeouts and output budgets elsewhere in that branch.
        let facts = self.collect_direct(|collector| collector.visit_expr_if(expression));
        self.check_facts("conditional", facts);
        visit::visit_expr_if(self, expression);
    }
}

struct LiteralCollector<'a> {
    facts: LiteralFacts,
    symbol_facts: &'a BTreeMap<String, LiteralFacts>,
}

impl<'a> LiteralCollector<'a> {
    fn new(symbol_facts: &'a BTreeMap<String, LiteralFacts>) -> Self {
        Self {
            facts: LiteralFacts::default(),
            symbol_facts,
        }
    }

    fn facts_for(&self, name: &str) -> Option<&LiteralFacts> {
        self.symbol_facts.get(name)
    }
}

impl<'ast> Visit<'ast> for LiteralCollector<'_> {
    fn visit_lit(&mut self, literal: &'ast Lit) {
        match literal {
            Lit::Str(value) if looks_like_model_id(&value.value()) => {
                self.facts.model_ids.insert(value.value());
            }
            Lit::Int(value) => {
                if let Ok(value) = value.base10_parse::<u64>()
                    && (8_000..=MAX_VALIDATED_CONTEXT_TOKENS).contains(&value)
                {
                    self.facts.capacity_literals.insert(value);
                }
            }
            _ => {}
        }
        visit::visit_lit(self, literal);
    }

    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if let Some(segment) = expression.path.segments.last()
            && let Some(facts) = self.facts_for(&segment.ident.to_string()).cloned()
        {
            self.facts.merge(facts);
        }
        visit::visit_expr_path(self, expression);
    }
}

fn looks_like_model_id(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    [
        "gpt-", "claude-", "gemini-", "qwen", "llama-", "deepseek", "gemma-", "gemma3", "gemma4",
        "minimax", "phi-", "phi4", "glm",
    ]
    .iter()
    .any(|prefix| value.starts_with(prefix))
}

fn has_test_cfg(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("test")
            || (attribute.path().is_ident("cfg")
                && match &attribute.meta {
                    syn::Meta::List(list) => list.tokens.to_string().contains("test"),
                    _ => false,
                })
    })
}

fn validate_documentation(
    input: &ValidationInput,
    snapshot: &CodexCatalogSnapshot,
    violations: &mut Vec<String>,
) {
    let expected = render_documentation(snapshot);
    match fs::read_to_string(&input.documentation_path) {
        Ok(actual) if normalize_newlines(&actual) == normalize_newlines(&expected) => {}
        Ok(_) => violations.push(format!(
            "{}: generated provider capability documentation drift",
            relative(&input.repo_root, &input.documentation_path)
        )),
        Err(error) => violations.push(format!(
            "{}: failed to read generated documentation: {error}",
            relative(&input.repo_root, &input.documentation_path)
        )),
    }
}

fn render_documentation(snapshot: &CodexCatalogSnapshot) -> String {
    let mut output = String::from(
        "# Provider capabilities\n\nThis file is generated from the daemon's typed provider-capability registry. Edit the registry or checked fixtures, then regenerate this document.\n\n",
    );
    output.push_str(GENERATED_START);
    output.push_str("\n\n## Provenance\n\n");
    output.push_str(&format!(
        "- Installed CLI: `{}`\n- Semantic fixture: `{}` (`{}`)\n- Schema coverage fixture: `{}` (39-key catalog)\n- Raw installed catalog: `{}` (acceptance artifact only)\n- Required consumer roles: `{}` (independent, non-generated acceptance anchor)\n",
        VALIDATED_CODEX_CLI_VERSION,
        SEMANTIC_FIXTURE_PATH,
        VALIDATED_CODEX_SEMANTIC_FIXTURE_DIGEST,
        SCHEMA_FIXTURE_PATH,
        VALIDATED_CODEX_RAW_CATALOG_DIGEST,
        REQUIRED_CONSUMER_ROLES_PATH,
    ));
    output.push_str("\nThe semantic fixture is bounded and omits provider prompt payloads. Its digest is intentionally distinct from the raw installed-catalog digest. The schema fixture keeps complete coverage of the 39 installed model fields. RSI preserves every visible installed-catalog entry and places current GPT-6 models first.\n\n");
    output.push_str("## RSI model projection\n\n");
    output.push_str("| Model | Display | Visibility / priority | Efforts (default first) | Default / effective / max context | Modalities | Tool mode / search |\n");
    output.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for model in &snapshot.models {
        let efforts = model
            .supported_reasoning_levels
            .iter()
            .map(|level| level.effort.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let default = model.capacity.provider_default_tokens.unwrap_or_default();
        let effective = model
            .capacity
            .provider_effective_default_tokens()
            .unwrap_or_default();
        let maximum = model.capacity.provider_max_tokens.unwrap_or_default();
        output.push_str(&format!(
            "| `{}` | {} | {} / {} | {} ({}) | {} / {} / {} | {} | {} / {} |\n",
            model.slug,
            model.display_name.as_deref().unwrap_or("—"),
            model.visibility.as_deref().unwrap_or("—"),
            model
                .priority
                .map_or_else(|| "—".to_string(), |value| value.to_string()),
            efforts,
            model.default_reasoning_level.as_deref().unwrap_or("—"),
            default,
            effective,
            maximum,
            model.tools.input_modalities.join(", "),
            model.tools.tool_mode.as_deref().unwrap_or("—"),
            if model.tools.supports_search_tool == Some(true) {
                model.tools.web_search_tool_type.as_deref().unwrap_or("yes")
            } else {
                "no"
            },
        ));
    }
    output.push_str("\n## Separate capacity sources\n\n");
    output.push_str("| Detail | Value | Source and authority |\n| --- | --- | --- |\n");
    for official in OFFICIAL_MODEL_CAPACITIES {
        output.push_str(&format!(
            "| `{}` advertised context | {} | [Official documentation]({}); descriptive, never an active CLI denominator by itself |\n",
            official.slug, official.advertised_max_tokens, official.documentation_url
        ));
        output.push_str(&format!(
            "| `{}` maximum output | {} | [Official documentation]({}); separate from installed catalog context |\n",
            official.slug, official.max_output_tokens, official.documentation_url
        ));
    }
    output.push_str("| Session compaction limit | runtime/configured when present | The installed 0.155.1 catalog omits a context-compaction threshold; `comp_hash` and `truncation_policy` are preserved metadata, not token capacity |\n");
    output.push_str("\n## Catalog field classification\n\n");
    output.push_str("| Field | Category | Handling | Detail |\n| --- | --- | --- | --- |\n");
    for contract in CODEX_CATALOG_MODEL_FIELDS {
        let (handling, detail) = match contract.handling {
            CatalogFieldHandling::Consumed(projection) => ("consumed", projection),
            CatalogFieldHandling::Ignored(reason) => ("ignored with reason", reason),
        };
        output.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            contract.key, contract.category, handling, detail
        ));
    }
    output.push_str("\n## Closed consumer inventory\n\n");
    output.push_str("| Role | Path | Symbol | Required dataflow |\n| --- | --- | --- | --- |\n");
    for consumer in CONSUMER_INVENTORY {
        let sinks = consumer
            .sinks
            .iter()
            .map(|sink| sink.describe())
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "| {} | `{}` | `{}` | `{}` → `{}` |\n",
            consumer.role.as_str(),
            consumer.path,
            consumer.symbol,
            consumer.source.describe(),
            sinks,
        ));
    }
    output.push('\n');
    output.push_str(GENERATED_END);
    output.push('\n');
    output
}

fn fixture_observed_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-03T00:00:00Z")
        .expect("fixture timestamp is valid")
        .with_timezone(&Utc)
}

fn sha256_digest(raw: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(raw)))
}

fn is_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n")
}

fn relative<'a>(repo_root: &Path, path: &'a Path) -> &'a str {
    path.strip_prefix(repo_root)
        .unwrap_or(path)
        .to_str()
        .unwrap_or("<non-utf8-path>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn production_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("repository root")
            .to_path_buf()
    }

    fn semantic_root_and_snapshot() -> (serde_json::Map<String, Value>, CodexCatalogSnapshot) {
        let raw = include_bytes!("../tests/fixtures/codex-models-0.155.1.json");
        let root = serde_json::from_slice::<Value>(raw)
            .expect("semantic fixture JSON")
            .as_object()
            .expect("semantic fixture object")
            .clone();
        let snapshot =
            parse_codex_catalog_snapshot(VALIDATED_CODEX_CLI_VERSION, raw, fixture_observed_at())
                .expect("semantic fixture projection");
        (root, snapshot)
    }

    #[test]
    fn production_repository_is_provider_capability_complete() {
        validate_repository(ValidationInput::production(production_root()))
            .expect("production provider capability contract");
    }

    #[test]
    fn catches_historical_name_and_effort_only_projection() {
        let (root, mut snapshot) = semantic_root_and_snapshot();
        snapshot.models[0].capacity.provider_default_tokens = None;
        snapshot.models[0].capacity.provider_max_tokens = None;
        snapshot.models[0].capacity.effective_percent = None;
        let mut violations = Vec::new();
        validate_semantic_projection(&root, &snapshot, &mut violations);
        assert!(violations.iter().any(|violation| {
            violation.contains("gpt-6-astra") && violation.contains("context_window")
        }));
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("max_context_window"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("effective_context_window_percent"))
        );
    }

    #[test]
    fn rejects_unknown_catalog_field_drift() {
        let (mut root, _) = semantic_root_and_snapshot();
        root.get_mut("models")
            .and_then(Value::as_array_mut)
            .and_then(|models| models.first_mut())
            .and_then(Value::as_object_mut)
            .expect("first model")
            .insert("future_capacity_hint".to_string(), Value::from(1));
        let mut violations = Vec::new();
        validate_root_and_model_keys(&root, false, "fixture", &mut violations);
        assert!(violations
            .iter()
            .any(|violation| violation.contains("unknown catalog field `future_capacity_hint`")));
    }

    #[test]
    fn rejects_missing_provenance() {
        let mut input = ValidationInput::production(production_root());
        input.expected_cli_version.clear();
        input.expected_raw_digest.clear();
        input.expected_semantic_digest.clear();
        let mut violations = Vec::new();
        validate_provenance(&input, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("CLI version"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("raw installed catalog provenance"))
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("semantic fixture provenance"))
        );
    }

    #[test]
    fn rejects_duplicate_context_mappings() {
        let mappings = &[("gpt-example", 10_000), ("GPT-EXAMPLE", 20_000)];
        let mut violations = Vec::new();
        validate_mapping_uniqueness("fixture", mappings, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("duplicates model pattern"))
        );
    }

    #[test]
    fn independent_role_anchor_rejects_inventory_deletion() {
        let root = tempfile::tempdir().expect("temporary root");
        let mut violations = Vec::new();
        validate_consumer_inventory(root.path(), &CONSUMER_INVENTORY[1..], &mut violations);
        assert!(violations.iter().any(|violation| {
            violation.contains("missing independently required role `startup-catalog-refresh`")
        }));
    }

    #[test]
    fn session_list_compact_row_contract_is_anchored_and_rejects_bypass() {
        let contract = *CONSUMER_INVENTORY
            .iter()
            .find(|consumer| consumer.role == ConsumerRole::SessionListCompactRow)
            .expect("actual session-list compact-row contract");

        let mut without_row = CONSUMER_INVENTORY.to_vec();
        without_row.retain(|consumer| consumer.role != ConsumerRole::SessionListCompactRow);
        let root = tempfile::tempdir().expect("temporary root");
        let mut violations = Vec::new();
        validate_consumer_inventory(root.path(), &without_row, &mut violations);
        assert!(violations.iter().any(|violation| {
            violation.contains("session-list-compact-row")
                && violation.contains("missing independently required role")
        }));

        let source_path = root.path().join(contract.path);
        fs::create_dir_all(source_path.parent().expect("source parent"))
            .expect("consumer fixture parent");
        let source = fs::read_to_string(production_root().join(contract.path))
            .expect("actual session-list row source");
        let bypassed = source.replacen(
            "match context.compact_label() {",
            "match None::<String> {",
            1,
        );
        assert_ne!(source, bypassed, "actual compact-label call replaced");
        fs::write(&source_path, bypassed).expect("bypassed session-list row fixture");

        let mut violations = Vec::new();
        validate_consumer_inventory(root.path(), &[contract], &mut violations);
        assert!(violations.iter().any(|violation| {
            violation.contains("session-list-compact-row")
                && violation.contains("call:compute_context_budget_view")
                && violation.contains("call:compact_label")
        }));
    }

    #[test]
    fn consumer_inventory_rejects_unused_source_decoy_inside_named_symbol() {
        let root = tempfile::tempdir().expect("temporary root");
        let source_path = root.path().join("consumer.rs");
        fs::write(
            &source_path,
            r#"
                fn target_consumer() {
                    let _decoy = resolved_context_budget();
                    persist(unrelated_value());
                }
            "#,
        )
        .expect("consumer fixture");
        let contract = ConsumerContract {
            role: ConsumerRole::StartupCatalogRefresh,
            path: "consumer.rs",
            symbol: "target_consumer",
            source: FlowSource::Call("resolved_context_budget"),
            sinks: &[FlowSink::Call("persist")],
        };
        let mut violations = Vec::new();
        validate_consumer_inventory(root.path(), &[contract], &mut violations);
        assert!(violations.iter().any(|violation| {
            violation
                .contains("no longer carries `call:resolved_context_budget` into `call:persist`")
        }));
    }

    #[test]
    fn consumer_inventory_accepts_source_to_sink_dataflow() {
        let file = syn::parse_file(
            r#"
                fn target_consumer() {
                    let budget = resolved_context_budget();
                    persist(Some(budget.clone()));
                }
            "#,
        )
        .expect("consumer fixture parses");
        let contract = ConsumerContract {
            role: ConsumerRole::StartupCatalogRefresh,
            path: "consumer.rs",
            symbol: "target_consumer",
            source: FlowSource::Call("resolved_context_budget"),
            sinks: &[FlowSink::Call("persist")],
        };
        let scope = consumer_scopes(&file, contract.symbol)
            .into_iter()
            .next()
            .expect("target consumer scope");
        let analysis = analyze_consumer_flow(scope, contract);
        assert!(analysis.source_present);
        assert_eq!(analysis.reached_sinks, [true]);
    }

    #[test]
    fn rejects_generated_documentation_drift() {
        let (_, snapshot) = semantic_root_and_snapshot();
        let temporary = tempfile::tempdir().expect("temporary root");
        let documentation = temporary.path().join("provider-capabilities.md");
        fs::write(&documentation, "stale generated content\n").expect("stale documentation");
        let mut input = ValidationInput::production(production_root());
        input.documentation_path = documentation;
        let mut violations = Vec::new();
        validate_documentation(&input, &snapshot, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("documentation drift"))
        );
    }

    #[test]
    fn source_scan_is_ast_aware_and_ignores_test_only_mappings() {
        let root = tempfile::tempdir().expect("temporary root");
        let source_dir = root.path().join("crates/example/src");
        fs::create_dir_all(&source_dir).expect("source fixture directory");
        fs::write(
            source_dir.join("lib.rs"),
            r#"
                fn leaked(model: &str) -> u64 {
                    match model { "gpt-future" => 456_000, _ => 128_000 }
                }
                #[cfg(test)]
                mod tests {
                    const ALLOWED_FIXTURE: &[(&str, u64)] = &[("gpt-test", 999_000)];
                }
            "#,
        )
        .expect("source fixture");
        let mut violations = Vec::new();
        scan_repository_sources(root.path(), &mut violations);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("gpt-future"));
        assert!(!violations[0].contains("gpt-test"));
    }

    #[test]
    fn source_scan_rejects_mapping_split_across_constants() {
        let root = tempfile::tempdir().expect("temporary root");
        let source_dir = root.path().join("crates/example/src");
        fs::create_dir_all(&source_dir).expect("source fixture directory");
        fs::write(
            source_dir.join("lib.rs"),
            r#"
                const MODEL: &str = "gpt-split";
                const CONTEXT: u64 = 456_000;

                fn leaked() -> (&'static str, u64) {
                    (MODEL, CONTEXT)
                }
            "#,
        )
        .expect("source fixture");
        let mut violations = Vec::new();
        scan_repository_sources(root.path(), &mut violations);
        assert!(
            violations.iter().any(|violation| {
                violation.contains("gpt-split") && violation.contains("456000")
            }),
            "{violations:?}"
        );
    }

    #[test]
    fn source_scan_rejects_helper_return_indirection() {
        let root = tempfile::tempdir().expect("temporary root");
        let source_dir = root.path().join("crates/example/src");
        fs::create_dir_all(&source_dir).expect("source fixture directory");
        fs::write(
            source_dir.join("lib.rs"),
            r#"
                fn selected_model() -> &'static str { "gpt-helper" }
                fn selected_context() -> u64 { 456_000 }

                fn leaked() -> (&'static str, u64) {
                    (selected_model(), selected_context())
                }
            "#,
        )
        .expect("source fixture");
        let mut violations = Vec::new();
        scan_repository_sources(root.path(), &mut violations);
        assert!(
            violations.iter().any(|violation| {
                violation.contains("gpt-helper") && violation.contains("456000")
            }),
            "{violations:?}"
        );
    }

    #[test]
    fn source_scan_rejects_map_insertion() {
        let root = tempfile::tempdir().expect("temporary root");
        let source_dir = root.path().join("crates/example/src");
        fs::create_dir_all(&source_dir).expect("source fixture directory");
        fs::write(
            source_dir.join("lib.rs"),
            r#"
                fn leaked() {
                    let mut capacities = std::collections::HashMap::new();
                    capacities.insert("gpt-map", 456_000);
                }
            "#,
        )
        .expect("source fixture");
        let mut violations = Vec::new();
        scan_repository_sources(root.path(), &mut violations);
        assert!(
            violations.iter().any(|violation| {
                violation.contains("map insertion")
                    && violation.contains("gpt-map")
                    && violation.contains("456000")
            }),
            "{violations:?}"
        );
    }

    #[test]
    fn source_scan_exempts_only_the_exact_canonical_module_path() {
        let root = tempfile::tempdir().expect("temporary root");
        let canonical = root.path().join("crates/rsid/src/provider_capabilities.rs");
        let same_named = root
            .path()
            .join("crates/example/src/provider_capabilities.rs");
        fs::create_dir_all(canonical.parent().expect("canonical parent"))
            .expect("canonical directory");
        fs::create_dir_all(same_named.parent().expect("same-named parent"))
            .expect("same-named directory");
        let mapping = r#"
            fn leaked(model: &str) -> u64 {
                match model { "gpt-same-name" => 456_000, _ => 128_000 }
            }
        "#;
        fs::write(&canonical, mapping).expect("canonical source fixture");
        fs::write(&same_named, mapping).expect("same-named source fixture");
        let mut violations = Vec::new();
        scan_repository_sources(root.path(), &mut violations);
        assert!(
            violations.iter().any(|violation| {
                violation.starts_with("crates/example/src/provider_capabilities.rs:")
            }),
            "{violations:?}"
        );
        assert!(
            violations.iter().all(|violation| {
                !violation.starts_with("crates/rsid/src/provider_capabilities.rs:")
            }),
            "{violations:?}"
        );
    }

    #[test]
    fn source_scan_rejects_obsolete_provider_specific_resolver_calls() {
        let root = tempfile::tempdir().expect("temporary root");
        let source_dir = root.path().join("crates/example/src");
        fs::create_dir_all(&source_dir).expect("source fixture directory");
        fs::write(
            source_dir.join("lib.rs"),
            r#"
                fn caller() -> u64 {
                    codex_cli_context_window_for_model("fixture")
                }
            "#,
        )
        .expect("source fixture");
        let mut violations = Vec::new();
        scan_repository_sources(root.path(), &mut violations);
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(violations[0].contains("codex_cli_context_window_for_model"));
    }

    #[test]
    fn schema_fixture_mechanically_covers_catalog_union() {
        let raw = include_bytes!("../tests/fixtures/codex-models-0.155.1-schema.json");
        let root = serde_json::from_slice::<Value>(raw)
            .expect("schema fixture JSON")
            .as_object()
            .expect("schema fixture object")
            .clone();
        let mut violations = Vec::new();
        validate_root_and_model_keys(&root, true, "schema", &mut violations);
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(CODEX_CATALOG_MODEL_FIELDS.len(), 39);
    }
}
