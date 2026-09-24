//! Dev-only recursive DAG smoke fixture generation.
//!
//! This module is gated by the `dev-fixtures` Cargo feature and is intended for
//! offline fixture DB preparation only. It never launches provider sessions and
//! only opens the generated or copied output DB through `Store::open`.

#![allow(clippy::too_many_lines)]

use crate::error::{DaemonError, Result};
use crate::recursive_dag::{
    RecursiveDagFakeBehavior, RecursiveDagFakeExecutor, RecursiveDagScheduler,
};
use crate::store::Store;
use crate::store::recursive_dag::{
    RecursiveExecutionArtifactCreate, RecursiveExecutionArtifactPreviewLookup,
    RecursiveExecutionArtifactPreviewOptions, RecursiveLiveAttemptCreate,
    RecursiveLiveOutputValidationCommit, RecursiveLiveOutputValidationReadOptions,
    RecursiveRootTaskCreate, RecursiveSchedulerLeasePolicy, RecursiveSchedulerRunStart,
    RecursiveTaskGraphCreate,
};
use chrono::Utc;
use rsi_common::recursive_dag::{
    RecursiveArtifactPreviewState, RecursiveAttemptId, RecursiveAttemptPhase,
    RecursiveExecutionArtifact, RecursiveExecutionArtifactKind, RecursiveExecutionMode,
    RecursiveLiveAttemptId, RecursiveLiveAttemptStatus, RecursiveLiveOutputKind,
    RecursiveLiveOutputMappingDecision, RecursiveLiveOutputParserSource,
    RecursiveLiveOutputRetryDecision, RecursiveLiveOutputRetryDecisionKind,
    RecursiveLiveOutputValidationArtifactLinks, RecursiveLiveOutputValidationId,
    RecursiveLiveOutputValidationIssue, RecursiveLiveOutputValidationResult,
    RecursiveLiveOutputValidationStatus, RecursiveLiveOutputValidationSummary,
    RecursiveLiveValidationIssueClass, RecursiveLiveValidationIssueCode,
    RecursiveLiveValidationIssueLocation, RecursiveLiveValidationIssueSeverity,
    RecursiveRecoveryBudget, RecursiveRecoveryPassId, RecursiveRecoveryPassStatus,
    RecursiveRecoverySource, RecursiveSchedulerRunId, RecursiveSchedulerRunSource,
    RecursiveSchedulerRunStatus, RecursiveSchedulerStopReason, RecursiveTaskGraphId,
    RecursiveTaskId, RecursiveTaskLifecycleState, RecursiveTopologyGraphCreateRequest,
};
use rsi_common::types::{
    FailurePolicy, SessionKind, Topology, TopologyDefinition, TopologyEdge, TopologyNode,
};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;
use uuid::Uuid;

pub const FIXTURE_SCHEMA_VERSION: &str = "recursive-dag-smoke-v1";
pub const GENERATOR_VERSION: &str = "1";
pub const MARKER_ARTIFACT_LABEL: &str = "smoke-fixture-marker";
pub const REVIEWED_FIXTURE_PLAN_COMMIT: &str = "329b914ddfd8c28f919c9fb5e1ab73b96cbd7c8a";
pub const SMOKE_HYGIENE_REVIEWED_SAFE_COMMIT: &str = "69a686f4740cd8f6eb2866067b81b8858b449935";

const FIXTURE_OPERATOR: &str = "recursive-dag-smoke-fixture";
const FIXTURE_NAMESPACE: Uuid = Uuid::from_u128(0xfd0d_10fb_fac7_42c5_b669_b2f14984e34c);
const PAGINATION_ARTIFACT_COUNT: usize = 75;
const CANCELLABLE_LEASE_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct SmokeFixtureOptions {
    pub source_db: Option<PathBuf>,
    pub output_db: PathBuf,
    pub output_home: Option<PathBuf>,
    pub summary_json: PathBuf,
    pub include_live_dogfood_graph: bool,
}

impl SmokeFixtureOptions {
    #[must_use]
    pub fn from_output_db(output_db: PathBuf, summary_json: PathBuf) -> Self {
        Self {
            source_db: None,
            output_db,
            output_home: None,
            summary_json,
            include_live_dogfood_graph: false,
        }
    }

    #[must_use]
    pub fn from_output_home(output_home: PathBuf) -> Self {
        let output_db = output_home.join(".rsi").join("rsi.db");
        let summary_json = output_home.join("recursive-dag-smoke-fixture.json");
        Self {
            source_db: None,
            output_db,
            output_home: Some(output_home),
            summary_json,
            include_live_dogfood_graph: false,
        }
    }

    #[must_use]
    pub fn with_source_db(mut self, source_db: PathBuf) -> Self {
        self.source_db = Some(source_db);
        self
    }

    #[must_use]
    pub fn with_live_dogfood_graph(mut self) -> Self {
        self.include_live_dogfood_graph = true;
        self
    }
}

#[derive(Debug, Clone)]
struct PreparedPaths {
    source_db: Option<PathBuf>,
    output_db: PathBuf,
    output_home: Option<PathBuf>,
    summary_json: PathBuf,
    include_live_dogfood_graph: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SmokeFixtureSummary {
    pub schema_version: String,
    pub generator_version: String,
    pub fixture_only: bool,
    pub reviewed_fixture_plan_commit: String,
    pub smoke_hygiene_reviewed_safe_commit: String,
    pub deterministic_namespace: String,
    pub generated_at: String,
    pub source_db: Option<String>,
    pub source_db_sha256_before: Option<String>,
    pub source_db_sha256_after: Option<String>,
    pub output_db: String,
    pub summary_json: String,
    pub cleanup_command: String,
    pub reused_existing: bool,
    pub created_new_output_db: bool,
    pub marker_artifact_id: Option<i64>,
    pub ids: SmokeFixtureIds,
    pub fixture_data: SmokeFixtureData,
    pub capability_expectations: SmokeCapabilityExpectations,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SmokeFixtureIds {
    pub topology_id: Uuid,
    pub topology_source_node_id: String,
    pub topology_source_iteration: u32,
    pub topology_graph_id: RecursiveTaskGraphId,
    pub topology_root_task_id: RecursiveTaskId,
    pub topology_first_task_id: RecursiveTaskId,
    pub topology_second_task_id: RecursiveTaskId,
    pub fake_scheduler_run_id: RecursiveSchedulerRunId,
    pub fake_scheduler_report_artifact_id: Option<i64>,
    pub manual_scheduler_graph_id: RecursiveTaskGraphId,
    pub manual_scheduler_root_task_id: RecursiveTaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_dogfood_graph_id: Option<RecursiveTaskGraphId>,
    pub cancellable_graph_id: RecursiveTaskGraphId,
    pub cancellable_root_task_id: RecursiveTaskId,
    pub cancellable_scheduler_run_id: RecursiveSchedulerRunId,
    pub recovery_pass_id: RecursiveRecoveryPassId,
    pub recovery_candidate_graph_ids: Vec<RecursiveTaskGraphId>,
    pub deferred_recovery_graph_id: Option<RecursiveTaskGraphId>,
    pub quarantine_graph_id: RecursiveTaskGraphId,
    pub quarantine_root_task_id: RecursiveTaskId,
    pub validation_graph_id: RecursiveTaskGraphId,
    pub validation_root_task_id: RecursiveTaskId,
    pub validation_scheduler_run_id: RecursiveSchedulerRunId,
    pub validation_attempt_id: RecursiveAttemptId,
    pub validation_live_attempt_id: RecursiveLiveAttemptId,
    pub validation_id: RecursiveLiveOutputValidationId,
    pub validation_raw_output_artifact_id: Option<i64>,
    pub validation_report_artifact_id: i64,
    pub validation_produced_artifact_ids: Vec<i64>,
    pub validation_test_artifact_ids: Vec<i64>,
    pub validation_diff_artifact_ids: Vec<i64>,
    pub pagination_artifact_ids: Vec<i64>,
    pub inline_preview_artifact_id: i64,
    pub external_blocked_artifact_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SmokeFixtureData {
    pub recursive_graph: bool,
    pub fake_scheduler_run: bool,
    pub cancellable_graph_run_target: bool,
    pub recovery_fixture_kind: String,
    pub topology_linked_recursive_fake_graph: bool,
    pub pagination_artifact_count: usize,
    pub inline_database_owned_preview_artifact: bool,
    pub blocked_uri_artifact: bool,
    pub validation_issue_count: u32,
    pub validation_error_count: u32,
    pub validation_warning_count: u32,
    pub quarantined_graph: bool,
    pub live_execution_launched: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SmokeCapabilityExpectations {
    pub recursive_dag_live_scheduler_control: bool,
    pub recursive_dag_live_execution: bool,
    pub recursive_dag_background_loop: bool,
    pub recursive_dag_test_inspection: bool,
    pub recursive_dag_diff_inspection: bool,
    pub recursive_dag_scheduler_report_inspection: bool,
    pub recursive_dag_safe_artifact_uri_open: bool,
}

#[derive(Debug, Clone)]
struct DeterministicIds {
    topology_id: Uuid,
    manual_scheduler_graph_id: RecursiveTaskGraphId,
    manual_scheduler_root_task_id: RecursiveTaskId,
    live_dogfood_graph_id: RecursiveTaskGraphId,
    live_dogfood_root_task_id: RecursiveTaskId,
    cancellable_graph_id: RecursiveTaskGraphId,
    cancellable_root_task_id: RecursiveTaskId,
    recovery_a_graph_id: RecursiveTaskGraphId,
    recovery_a_root_task_id: RecursiveTaskId,
    recovery_a_attempt_id: RecursiveAttemptId,
    recovery_b_graph_id: RecursiveTaskGraphId,
    recovery_b_root_task_id: RecursiveTaskId,
    recovery_b_attempt_id: RecursiveAttemptId,
    quarantine_graph_id: RecursiveTaskGraphId,
    quarantine_root_task_id: RecursiveTaskId,
    validation_graph_id: RecursiveTaskGraphId,
    validation_root_task_id: RecursiveTaskId,
    validation_attempt_id: RecursiveAttemptId,
    validation_live_attempt_id: RecursiveLiveAttemptId,
}

pub fn generate_smoke_fixture(options: SmokeFixtureOptions) -> Result<SmokeFixtureSummary> {
    let paths = prepare_paths(options)?;
    let output_existed_before = paths.output_db.exists();
    if output_existed_before {
        validate_existing_output_sidecar(&paths)?;
    }
    let source_hash_before = paths.source_db.as_deref().map(sha256_file).transpose()?;

    if !output_existed_before {
        if let Some(parent) = paths.output_db.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(source_db) = paths.source_db.as_deref() {
            copy_sqlite_db_readonly(source_db, &paths.output_db)?;
        }
    }

    let source_hash_after = paths.source_db.as_deref().map(sha256_file).transpose()?;
    if source_hash_before.is_some() && source_hash_before != source_hash_after {
        return Err(DaemonError::Store(
            "source DB checksum changed during fixture generation".to_string(),
        ));
    }

    let store = Store::open(&paths.output_db)?;
    if let Some(summary) = load_existing_fixture_summary(
        &store,
        &paths,
        source_hash_before.as_deref(),
        source_hash_after.as_deref(),
        !output_existed_before,
    )? {
        return Ok(summary);
    }
    ensure_no_fixture_collisions(&store, paths.include_live_dogfood_graph)?;

    build_fixture(
        &store,
        &paths,
        source_hash_before,
        source_hash_after,
        !output_existed_before,
    )
}

fn prepare_paths(options: SmokeFixtureOptions) -> Result<PreparedPaths> {
    if options.output_db.as_os_str().is_empty() {
        return Err(DaemonError::InvalidParam(
            "explicit --output-db or --output-home is required".to_string(),
        ));
    }
    if options.output_db.exists() && options.output_db.is_dir() {
        return Err(DaemonError::InvalidParam(format!(
            "output DB path is a directory: {}",
            options.output_db.display()
        )));
    }
    if let Some(source_db) = options.source_db.as_deref() {
        if !source_db.exists() {
            return Err(DaemonError::InvalidParam(format!(
                "source DB does not exist: {}",
                source_db.display()
            )));
        }
        if source_db.is_dir() {
            return Err(DaemonError::InvalidParam(format!(
                "source DB path is a directory: {}",
                source_db.display()
            )));
        }
    }

    let output_compare = comparable_path(&options.output_db)?;
    if let Some(default_db) = default_home_db_path()
        && output_compare == comparable_path(&default_db)?
    {
        return Err(DaemonError::InvalidParam(format!(
            "refusing to write fixture data to default production DB path: {}",
            default_db.display()
        )));
    }
    if let Some(source_db) = options.source_db.as_deref()
        && comparable_path(source_db)? == output_compare
    {
        return Err(DaemonError::InvalidParam(
            "source DB and output DB must be different paths".to_string(),
        ));
    }

    Ok(PreparedPaths {
        source_db: options.source_db.map(absolutize),
        output_db: absolutize(options.output_db),
        output_home: options.output_home.map(absolutize),
        summary_json: absolutize(options.summary_json),
        include_live_dogfood_graph: options.include_live_dogfood_graph,
    })
}

fn default_home_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".rsi").join("rsi.db"))
}

fn comparable_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    if let Some(parent) = path.parent()
        && parent.exists()
    {
        return Ok(parent.canonicalize()?.join(path.file_name().ok_or_else(|| {
            DaemonError::InvalidParam(format!("path has no file name: {}", path.display()))
        })?));
    }
    Ok(absolutize(path))
}

fn absolutize(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn copy_sqlite_db_readonly(source_db: &Path, output_db: &Path) -> Result<()> {
    let source = Connection::open_with_flags(
        source_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut destination = Connection::open(output_db)?;
    let backup = Backup::new(&source, &mut destination)?;
    backup.run_to_completion(16, StdDuration::from_millis(50), None)?;
    Ok(())
}

fn validate_existing_output_sidecar(paths: &PreparedPaths) -> Result<()> {
    if !paths.summary_json.exists() {
        return Err(DaemonError::InvalidParam(format!(
            "output DB already exists without fixture summary JSON; use a new output path or rerun an existing fixture: {}",
            paths.output_db.display()
        )));
    }

    let content = fs::read_to_string(&paths.summary_json)?;
    let summary = serde_json::from_str::<SmokeFixtureSummary>(&content)?;
    if !summary.fixture_only || summary.schema_version != FIXTURE_SCHEMA_VERSION {
        return Err(DaemonError::InvalidParam(format!(
            "output DB already exists but summary is not a {FIXTURE_SCHEMA_VERSION} fixture: {}",
            paths.summary_json.display()
        )));
    }
    let summary_output = PathBuf::from(&summary.output_db);
    if comparable_path(&summary_output)? != comparable_path(&paths.output_db)? {
        return Err(DaemonError::InvalidParam(format!(
            "output DB path does not match fixture summary: {}",
            paths.summary_json.display()
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn load_existing_fixture_summary(
    store: &Store,
    paths: &PreparedPaths,
    source_hash_before: Option<&str>,
    source_hash_after: Option<&str>,
    created_new_output_db: bool,
) -> Result<Option<SmokeFixtureSummary>> {
    let ids = deterministic_ids();
    let Some(_) = store.get_recursive_task_graph(ids.cancellable_graph_id)? else {
        return Ok(None);
    };
    let marker = load_marker_artifact(store, ids.cancellable_graph_id)?;
    let Some(marker) = marker else {
        return Err(DaemonError::Store(format!(
            "fixture collision: deterministic graph {} exists but marker artifact is missing; discard the smoke DB",
            ids.cancellable_graph_id
        )));
    };

    let use_sidecar = !created_new_output_db && paths.summary_json.exists();
    let mut summary = if use_sidecar {
        let content = fs::read_to_string(&paths.summary_json)?;
        serde_json::from_str::<SmokeFixtureSummary>(&content)?
    } else {
        let content = marker.content.as_deref().ok_or_else(|| {
            DaemonError::Store("fixture marker artifact has no content".to_string())
        })?;
        serde_json::from_str::<SmokeFixtureSummary>(content)?
    };
    if summary.schema_version != FIXTURE_SCHEMA_VERSION {
        return Err(DaemonError::Store(format!(
            "fixture marker schema mismatch: found {}, expected {FIXTURE_SCHEMA_VERSION}",
            summary.schema_version
        )));
    }
    summary.reused_existing = true;
    summary.created_new_output_db = created_new_output_db;
    summary.marker_artifact_id = Some(marker.id);
    if !use_sidecar {
        summary.source_db = paths
            .source_db
            .as_ref()
            .map(|path| path.display().to_string());
        summary.source_db_sha256_before = source_hash_before.map(ToString::to_string);
        summary.source_db_sha256_after = source_hash_after.map(ToString::to_string);
        summary.output_db = paths.output_db.display().to_string();
        summary.summary_json = paths.summary_json.display().to_string();
        summary.cleanup_command = cleanup_command(paths);
    }
    let live_dogfood_summary_changed = if paths.include_live_dogfood_graph {
        ensure_live_dogfood_graph_for_summary(store, &mut summary)?
    } else {
        false
    };
    if live_dogfood_summary_changed {
        record_fixture_marker(store, &mut summary)?;
    }
    if !use_sidecar || live_dogfood_summary_changed {
        write_summary_json(paths, &summary)?;
    }
    validate_existing_fixture(store, &summary)?;
    Ok(Some(summary))
}

fn ensure_live_dogfood_graph_for_summary(
    store: &Store,
    summary: &mut SmokeFixtureSummary,
) -> Result<bool> {
    let ids = deterministic_ids();
    match summary.ids.live_dogfood_graph_id {
        Some(graph_id) => {
            if graph_id != ids.live_dogfood_graph_id {
                return Err(DaemonError::Store(format!(
                    "fixture summary live dogfood graph id {graph_id} does not match deterministic id {}",
                    ids.live_dogfood_graph_id
                )));
            }
            require_live_dogfood_graph(store, graph_id)?;
            Ok(false)
        }
        None => {
            if store
                .get_recursive_task_graph(ids.live_dogfood_graph_id)?
                .is_none()
            {
                create_live_dogfood_fixture(store, &ids)?;
            }
            require_live_dogfood_graph(store, ids.live_dogfood_graph_id)?;
            summary.ids.live_dogfood_graph_id = Some(ids.live_dogfood_graph_id);
            Ok(true)
        }
    }
}

fn load_marker_artifact(
    store: &Store,
    marker_graph_id: RecursiveTaskGraphId,
) -> Result<Option<RecursiveExecutionArtifact>> {
    let artifacts = store.load_recursive_execution_artifacts(marker_graph_id)?;
    Ok(artifacts
        .into_iter()
        .filter(|artifact| artifact.label == MARKER_ARTIFACT_LABEL)
        .max_by_key(|artifact| artifact.id))
}

fn validate_existing_fixture(store: &Store, summary: &SmokeFixtureSummary) -> Result<()> {
    let ids = &summary.ids;
    require_graph(store, ids.topology_graph_id)?;
    require_graph(store, ids.manual_scheduler_graph_id)?;
    if let Some(live_graph_id) = ids.live_dogfood_graph_id {
        require_live_dogfood_graph(store, live_graph_id)?;
    }
    require_graph(store, ids.cancellable_graph_id)?;
    require_graph(store, ids.validation_graph_id)?;
    require_graph(store, ids.quarantine_graph_id)?;
    for graph_id in &ids.recovery_candidate_graph_ids {
        require_graph(store, *graph_id)?;
    }
    require_scheduler_run(store, ids.fake_scheduler_run_id)?;
    require_scheduler_run(store, ids.cancellable_scheduler_run_id)?;
    require_scheduler_run(store, ids.validation_scheduler_run_id)?;
    require_artifact_preview_state(
        store,
        ids.topology_graph_id,
        ids.inline_preview_artifact_id,
        RecursiveArtifactPreviewState::Available,
    )?;
    require_artifact_preview_state(
        store,
        ids.topology_graph_id,
        ids.external_blocked_artifact_id,
        RecursiveArtifactPreviewState::UriBlocked,
    )?;
    let validation = store
        .load_recursive_live_output_validation_result(
            ids.validation_id,
            RecursiveLiveOutputValidationReadOptions {
                include_issues: true,
                include_normalized_output: false,
                include_validation_report: true,
            },
        )?
        .ok_or_else(|| {
            DaemonError::Store(format!(
                "fixture validation row missing: {}",
                ids.validation_id
            ))
        })?;
    if validation.issues.is_empty() {
        return Err(DaemonError::Store(
            "fixture validation row has no issues".to_string(),
        ));
    }
    Ok(())
}

fn require_graph(store: &Store, graph_id: RecursiveTaskGraphId) -> Result<()> {
    store
        .get_recursive_task_graph(graph_id)?
        .ok_or_else(|| DaemonError::Store(format!("fixture graph missing: {graph_id}")))?;
    Ok(())
}

fn require_live_dogfood_graph(store: &Store, graph_id: RecursiveTaskGraphId) -> Result<()> {
    let graph = store
        .get_recursive_task_graph(graph_id)?
        .ok_or_else(|| DaemonError::Store(format!("fixture live graph missing: {graph_id}")))?;
    if graph.graph.execution_mode != RecursiveExecutionMode::LiveSession {
        return Err(DaemonError::Store(format!(
            "fixture live graph {graph_id} has execution mode {:?}, expected {:?}",
            graph.graph.execution_mode,
            RecursiveExecutionMode::LiveSession
        )));
    }
    if graph.graph.topology_id.is_some() {
        return Err(DaemonError::Store(format!(
            "fixture live graph {graph_id} is topology-linked; expected ordinary graph"
        )));
    }
    Ok(())
}

fn require_scheduler_run(store: &Store, run_id: RecursiveSchedulerRunId) -> Result<()> {
    store
        .load_recursive_scheduler_run(run_id)?
        .ok_or_else(|| DaemonError::Store(format!("fixture scheduler run missing: {run_id}")))?;
    Ok(())
}

fn require_artifact_preview_state(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
    artifact_id: i64,
    expected: RecursiveArtifactPreviewState,
) -> Result<()> {
    match store.preview_recursive_execution_artifact_by_id(
        graph_id,
        artifact_id,
        RecursiveExecutionArtifactPreviewOptions::default(),
    )? {
        RecursiveExecutionArtifactPreviewLookup::Found(preview)
            if preview.content_state == expected =>
        {
            Ok(())
        }
        RecursiveExecutionArtifactPreviewLookup::Found(preview) => {
            Err(DaemonError::Store(format!(
                "fixture artifact {artifact_id} preview state {:?}, expected {:?}",
                preview.content_state, expected
            )))
        }
        RecursiveExecutionArtifactPreviewLookup::NotFound => Err(DaemonError::Store(format!(
            "fixture artifact missing: {artifact_id}"
        ))),
        RecursiveExecutionArtifactPreviewLookup::GraphMismatch { actual_graph_id } => {
            Err(DaemonError::Store(format!(
                "fixture artifact {artifact_id} belongs to graph {actual_graph_id}, not {graph_id}"
            )))
        }
    }
}

fn ensure_no_fixture_collisions(store: &Store, include_live_dogfood_graph: bool) -> Result<()> {
    let ids = deterministic_ids();
    let mut graph_ids = vec![
        ids.manual_scheduler_graph_id,
        ids.cancellable_graph_id,
        ids.recovery_a_graph_id,
        ids.recovery_b_graph_id,
        ids.quarantine_graph_id,
        ids.validation_graph_id,
    ];
    if include_live_dogfood_graph {
        graph_ids.push(ids.live_dogfood_graph_id);
    }
    for graph_id in graph_ids {
        if store.get_recursive_task_graph(graph_id)?.is_some() {
            return Err(DaemonError::Store(format!(
                "fixture collision: deterministic graph {graph_id} already exists without a matching marker"
            )));
        }
    }
    if store.get_topology(ids.topology_id)?.is_some() {
        return Err(DaemonError::Store(format!(
            "fixture collision: deterministic topology {} already exists without a matching marker",
            ids.topology_id
        )));
    }
    if store
        .load_recursive_live_attempt(ids.validation_live_attempt_id)?
        .is_some()
    {
        return Err(DaemonError::Store(format!(
            "fixture collision: deterministic live attempt {} already exists without a matching marker",
            ids.validation_live_attempt_id
        )));
    }
    Ok(())
}

fn build_fixture(
    store: &Store,
    paths: &PreparedPaths,
    source_hash_before: Option<String>,
    source_hash_after: Option<String>,
    created_new_output_db: bool,
) -> Result<SmokeFixtureSummary> {
    let ids = deterministic_ids();
    let recovery = create_recovery_fixture(store, &ids)?;
    let quarantine = create_quarantine_fixture(store, &ids)?;
    let topology = create_topology_fake_fixture(store, &ids)?;
    let manual = create_manual_scheduler_target(store, &ids)?;
    let live_dogfood = if paths.include_live_dogfood_graph {
        Some(create_live_dogfood_fixture(store, &ids)?)
    } else {
        None
    };
    let cancellable = create_cancellable_fixture(store, &ids)?;
    let validation = create_validation_fixture(store, &ids)?;

    let mut summary = SmokeFixtureSummary {
        schema_version: FIXTURE_SCHEMA_VERSION.to_string(),
        generator_version: GENERATOR_VERSION.to_string(),
        fixture_only: true,
        reviewed_fixture_plan_commit: REVIEWED_FIXTURE_PLAN_COMMIT.to_string(),
        smoke_hygiene_reviewed_safe_commit: SMOKE_HYGIENE_REVIEWED_SAFE_COMMIT.to_string(),
        deterministic_namespace: FIXTURE_NAMESPACE.to_string(),
        generated_at: Utc::now().to_rfc3339(),
        source_db: paths
            .source_db
            .as_ref()
            .map(|path| path.display().to_string()),
        source_db_sha256_before: source_hash_before,
        source_db_sha256_after: source_hash_after,
        output_db: paths.output_db.display().to_string(),
        summary_json: paths.summary_json.display().to_string(),
        cleanup_command: cleanup_command(paths),
        reused_existing: false,
        created_new_output_db,
        marker_artifact_id: None,
        ids: SmokeFixtureIds {
            topology_id: ids.topology_id,
            topology_source_node_id: "implementation".to_string(),
            topology_source_iteration: 0,
            topology_graph_id: topology.graph_id,
            topology_root_task_id: topology.root_task_id,
            topology_first_task_id: topology.first_task_id,
            topology_second_task_id: topology.second_task_id,
            fake_scheduler_run_id: topology.scheduler_run_id,
            fake_scheduler_report_artifact_id: topology.scheduler_report_artifact_id,
            manual_scheduler_graph_id: manual.graph_id,
            manual_scheduler_root_task_id: manual.root_task_id,
            live_dogfood_graph_id: live_dogfood.as_ref().map(|fixture| fixture.graph_id),
            cancellable_graph_id: cancellable.graph_id,
            cancellable_root_task_id: cancellable.root_task_id,
            cancellable_scheduler_run_id: cancellable.run_id,
            recovery_pass_id: recovery.pass_id,
            recovery_candidate_graph_ids: recovery.candidate_graph_ids,
            deferred_recovery_graph_id: recovery.deferred_graph_id,
            quarantine_graph_id: quarantine.graph_id,
            quarantine_root_task_id: quarantine.root_task_id,
            validation_graph_id: validation.graph_id,
            validation_root_task_id: validation.root_task_id,
            validation_scheduler_run_id: validation.scheduler_run_id,
            validation_attempt_id: validation.attempt_id,
            validation_live_attempt_id: validation.live_attempt_id,
            validation_id: validation.validation_id,
            validation_raw_output_artifact_id: validation.raw_output_artifact_id,
            validation_report_artifact_id: validation.validation_report_artifact_id,
            validation_produced_artifact_ids: validation.produced_artifact_ids,
            validation_test_artifact_ids: validation.test_artifact_ids,
            validation_diff_artifact_ids: validation.diff_artifact_ids,
            pagination_artifact_ids: topology.pagination_artifact_ids,
            inline_preview_artifact_id: topology.inline_preview_artifact_id,
            external_blocked_artifact_id: topology.external_blocked_artifact_id,
        },
        fixture_data: SmokeFixtureData {
            recursive_graph: true,
            fake_scheduler_run: true,
            cancellable_graph_run_target: true,
            recovery_fixture_kind: if recovery.deferred_graph_id.is_some() {
                "deferred".to_string()
            } else {
                "no_op".to_string()
            },
            topology_linked_recursive_fake_graph: true,
            pagination_artifact_count: PAGINATION_ARTIFACT_COUNT,
            inline_database_owned_preview_artifact: true,
            blocked_uri_artifact: true,
            validation_issue_count: validation.issue_count,
            validation_error_count: validation.error_count,
            validation_warning_count: validation.warning_count,
            quarantined_graph: true,
            live_execution_launched: false,
        },
        capability_expectations: SmokeCapabilityExpectations {
            recursive_dag_live_scheduler_control: false,
            recursive_dag_live_execution: false,
            recursive_dag_background_loop: false,
            recursive_dag_test_inspection: false,
            recursive_dag_diff_inspection: false,
            recursive_dag_scheduler_report_inspection: false,
            recursive_dag_safe_artifact_uri_open: false,
        },
    };

    record_fixture_marker(store, &mut summary)?;
    write_summary_json(paths, &summary)?;
    validate_existing_fixture(store, &summary)?;
    Ok(summary)
}

fn record_fixture_marker(store: &Store, summary: &mut SmokeFixtureSummary) -> Result<()> {
    let marker_content = serde_json::to_string_pretty(summary)?;
    let marker = store.record_recursive_execution_artifacts(
        summary.ids.cancellable_graph_id,
        vec![RecursiveExecutionArtifactCreate {
            task_id: summary.ids.cancellable_root_task_id,
            attempt_id: None,
            kind: RecursiveExecutionArtifactKind::Inline,
            label: MARKER_ARTIFACT_LABEL.to_string(),
            content: Some(marker_content),
            uri: None,
            metadata: fixture_metadata(json!({
                "artifact_role": "fixture_marker",
            })),
        }],
    )?;
    let marker_id = marker.first().map(|artifact| artifact.id).ok_or_else(|| {
        DaemonError::Store("fixture marker artifact was not inserted".to_string())
    })?;
    summary.marker_artifact_id = Some(marker_id);
    Ok(())
}

#[derive(Debug)]
struct RecoveryFixture {
    pass_id: RecursiveRecoveryPassId,
    candidate_graph_ids: Vec<RecursiveTaskGraphId>,
    deferred_graph_id: Option<RecursiveTaskGraphId>,
}

fn create_recovery_fixture(store: &Store, ids: &DeterministicIds) -> Result<RecoveryFixture> {
    create_graph(
        store,
        ids.recovery_a_graph_id,
        ids.recovery_a_root_task_id,
        "Smoke recovery candidate A",
        "Recover an in-flight fixture task after restart",
    )?;
    create_graph(
        store,
        ids.recovery_b_graph_id,
        ids.recovery_b_root_task_id,
        "Smoke recovery candidate B",
        "Remain deferred after the one-graph recovery budget",
    )?;
    make_in_flight_recovery_candidate(
        store,
        ids.recovery_a_graph_id,
        ids.recovery_a_root_task_id,
        ids.recovery_a_attempt_id,
    )?;
    make_in_flight_recovery_candidate(
        store,
        ids.recovery_b_graph_id,
        ids.recovery_b_root_task_id,
        ids.recovery_b_attempt_id,
    )?;

    let pass =
        store.recover_recursive_task_graphs_after_restart_with_budget(RecursiveRecoveryBudget {
            max_graphs: 1,
            time_budget_ms: None,
            source: RecursiveRecoverySource::Startup,
        })?;
    let deferred = store.list_deferred_recursive_recovery_graphs()?;
    let deferred_graph_id = deferred.iter().find_map(|graph| {
        graph.graph_id.filter(|graph_id| {
            *graph_id == ids.recovery_a_graph_id || *graph_id == ids.recovery_b_graph_id
        })
    });
    if pass.status != RecursiveRecoveryPassStatus::Deferred || deferred_graph_id.is_none() {
        return Err(DaemonError::Store(
            "recovery smoke fixture did not produce deferred work".to_string(),
        ));
    }

    Ok(RecoveryFixture {
        pass_id: pass.id,
        candidate_graph_ids: vec![ids.recovery_a_graph_id, ids.recovery_b_graph_id],
        deferred_graph_id,
    })
}

fn make_in_flight_recovery_candidate(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
    task_id: RecursiveTaskId,
    attempt_id: RecursiveAttemptId,
) -> Result<()> {
    store.record_recursive_attempt_start(
        graph_id,
        task_id,
        RecursiveAttemptPhase::Execute,
        attempt_id,
    )?;
    store.transition_recursive_task_state(
        graph_id,
        task_id,
        RecursiveTaskLifecycleState::Planning,
        Some("fixture recovery candidate: planning before simulated restart".to_string()),
    )?;
    store.transition_recursive_task_state(
        graph_id,
        task_id,
        RecursiveTaskLifecycleState::Running,
        Some("fixture recovery candidate: running at simulated restart".to_string()),
    )?;
    Ok(())
}

#[derive(Debug)]
struct QuarantineFixture {
    graph_id: RecursiveTaskGraphId,
    root_task_id: RecursiveTaskId,
}

fn create_quarantine_fixture(store: &Store, ids: &DeterministicIds) -> Result<QuarantineFixture> {
    create_graph(
        store,
        ids.quarantine_graph_id,
        ids.quarantine_root_task_id,
        "Smoke quarantined graph",
        "Render safe Store-owned quarantine metadata",
    )?;
    store.quarantine_recursive_task_graph(
        ids.quarantine_graph_id,
        "recursive DAG smoke fixture quarantine target".to_string(),
    )?;
    Ok(QuarantineFixture {
        graph_id: ids.quarantine_graph_id,
        root_task_id: ids.quarantine_root_task_id,
    })
}

#[derive(Debug)]
struct TopologyFixture {
    graph_id: RecursiveTaskGraphId,
    root_task_id: RecursiveTaskId,
    first_task_id: RecursiveTaskId,
    second_task_id: RecursiveTaskId,
    scheduler_run_id: RecursiveSchedulerRunId,
    scheduler_report_artifact_id: Option<i64>,
    pagination_artifact_ids: Vec<i64>,
    inline_preview_artifact_id: i64,
    external_blocked_artifact_id: i64,
}

fn create_topology_fake_fixture(store: &Store, ids: &DeterministicIds) -> Result<TopologyFixture> {
    let topology = smoke_topology(ids.topology_id);
    store.insert_topology(&topology)?;
    let response =
        store.create_recursive_graph_from_topology_node(RecursiveTopologyGraphCreateRequest {
            topology_id: ids.topology_id,
            node_id: "implementation".to_string(),
            topology_iteration: 0,
            idempotency_key: Some(format!("{FIXTURE_SCHEMA_VERSION}:topology-graph")),
            project_id: None,
            parent_session_id: None,
            workflow_id: None,
            workflow_execution_id: None,
            include_prerequisite_closure: true,
            max_depth: Some(4),
            max_fanout: Some(8),
            max_descendants: Some(32),
            step_limit: Some(16),
            policy_overrides: Some(fixture_metadata(json!({
                "fixture_note": "topology-linked fake graph; no live execution",
            }))),
        })?;
    let first_task_id = task_link_id(&response, "planning")?;
    let second_task_id = task_link_id(&response, "implementation")?;
    let artifacts = create_topology_artifacts(store, response.graph.id, response.root_task_id)?;

    let mut scheduler = RecursiveDagScheduler::new(RecursiveDagFakeExecutor::new().on_execute(
        first_task_id,
        RecursiveDagFakeBehavior::direct_success("smoke-topology-planning-result"),
    ));
    let report = scheduler.run_until_idle_with_options(
        store,
        response.graph.id,
        1,
        RecursiveSchedulerRunSource::TestHarness,
        Some(FIXTURE_OPERATOR.to_string()),
        RecursiveSchedulerLeasePolicy {
            lease_owner: FIXTURE_OPERATOR.to_string(),
            lease_ttl_seconds: 60,
            max_active_runs: 16,
        },
    )?;
    let run = store
        .load_recursive_scheduler_run(report.run_id)?
        .ok_or_else(|| {
            DaemonError::Store(format!(
                "topology fixture scheduler run missing after run: {}",
                report.run_id
            ))
        })?;
    if run.status != RecursiveSchedulerRunStatus::Completed {
        return Err(DaemonError::Store(format!(
            "topology fixture scheduler run did not complete: {:?}",
            run.status
        )));
    }

    Ok(TopologyFixture {
        graph_id: response.graph.id,
        root_task_id: response.root_task_id,
        first_task_id,
        second_task_id,
        scheduler_run_id: report.run_id,
        scheduler_report_artifact_id: run.report_artifact_id,
        pagination_artifact_ids: artifacts.pagination_artifact_ids,
        inline_preview_artifact_id: artifacts.inline_preview_artifact_id,
        external_blocked_artifact_id: artifacts.external_blocked_artifact_id,
    })
}

fn smoke_topology(topology_id: Uuid) -> Topology {
    let now = Utc::now();
    let planning_params = HashMap::from([(
        "instructions".to_string(),
        json!("Fixture-only planning node for recursive DAG smoke data."),
    )]);
    let implementation_params = HashMap::from([(
        "instructions".to_string(),
        json!("Fixture-only implementation node; no provider session is launched."),
    )]);
    Topology {
        id: topology_id,
        name: "Recursive DAG smoke fixture topology".to_string(),
        definition: TopologyDefinition {
            nodes: vec![
                TopologyNode {
                    id: "planning".to_string(),
                    kind: SessionKind::Task,
                    label: "Smoke planning task".to_string(),
                    prereqs: Vec::new(),
                    max_iterations: Some(1),
                    on_failure: Some(FailurePolicy::Halt),
                    params: planning_params,
                },
                TopologyNode {
                    id: "implementation".to_string(),
                    kind: SessionKind::Task,
                    label: "Smoke implementation task".to_string(),
                    prereqs: vec!["planning".to_string()],
                    max_iterations: Some(1),
                    on_failure: Some(FailurePolicy::Halt),
                    params: implementation_params,
                },
            ],
            edges: vec![TopologyEdge {
                from: "planning".to_string(),
                to: "implementation".to_string(),
                loop_edge: false,
            }],
            until: None,
        },
        created_at: now,
        updated_at: now,
    }
}

fn task_link_id(
    response: &rsi_common::recursive_dag::RecursiveTopologyGraphCreateResponse,
    node_id: &str,
) -> Result<RecursiveTaskId> {
    response
        .task_links
        .iter()
        .find(|link| link.topology_node_id == node_id)
        .map(|link| link.task_id)
        .ok_or_else(|| {
            DaemonError::Store(format!(
                "topology fixture task link missing for node {node_id}"
            ))
        })
}

struct TopologyArtifacts {
    pagination_artifact_ids: Vec<i64>,
    inline_preview_artifact_id: i64,
    external_blocked_artifact_id: i64,
}

fn create_topology_artifacts(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
    root_task_id: RecursiveTaskId,
) -> Result<TopologyArtifacts> {
    let mut creates = Vec::with_capacity(PAGINATION_ARTIFACT_COUNT + 2);
    for index in 0..PAGINATION_ARTIFACT_COUNT {
        creates.push(RecursiveExecutionArtifactCreate {
            task_id: root_task_id,
            attempt_id: None,
            kind: RecursiveExecutionArtifactKind::Inline,
            label: format!("smoke-page-{index:03}"),
            content: Some(format!(
                "{{\"fixture\":\"{FIXTURE_SCHEMA_VERSION}\",\"page_index\":{index}}}"
            )),
            uri: None,
            metadata: fixture_metadata(json!({
                "artifact_role": "pagination",
                "page_index": index,
            })),
        });
    }
    creates.push(RecursiveExecutionArtifactCreate {
        task_id: root_task_id,
        attempt_id: None,
        kind: RecursiveExecutionArtifactKind::Inline,
        label: "smoke-inline-preview".to_string(),
        content: Some(
            json!({
                "fixture": FIXTURE_SCHEMA_VERSION,
                "preview": "database-owned bounded inline content",
                "live_execution": false,
            })
            .to_string(),
        ),
        uri: None,
        metadata: fixture_metadata(json!({
            "artifact_role": "inline_preview",
        })),
    });
    creates.push(RecursiveExecutionArtifactCreate {
        task_id: root_task_id,
        attempt_id: None,
        kind: RecursiveExecutionArtifactKind::File,
        label: "smoke-external-blocked".to_string(),
        content: Some("shadow content that must not be returned while uri is present".to_string()),
        uri: Some("file:///tmp/rsi-recursive-dag-smoke-blocked.txt".to_string()),
        metadata: fixture_metadata(json!({
            "artifact_role": "blocked_uri_preview",
            "expected_preview_state": "uri_blocked",
        })),
    });

    let inserted = store.record_recursive_execution_artifacts(graph_id, creates)?;
    let mut pagination_artifact_ids = Vec::new();
    let mut inline_preview_artifact_id = None;
    let mut external_blocked_artifact_id = None;
    for artifact in inserted {
        if artifact.label.starts_with("smoke-page-") {
            pagination_artifact_ids.push(artifact.id);
        } else if artifact.label == "smoke-inline-preview" {
            inline_preview_artifact_id = Some(artifact.id);
        } else if artifact.label == "smoke-external-blocked" {
            external_blocked_artifact_id = Some(artifact.id);
        }
    }
    if pagination_artifact_ids.len() != PAGINATION_ARTIFACT_COUNT {
        return Err(DaemonError::Store(
            "pagination fixture artifact count mismatch".to_string(),
        ));
    }
    Ok(TopologyArtifacts {
        pagination_artifact_ids,
        inline_preview_artifact_id: inline_preview_artifact_id.ok_or_else(|| {
            DaemonError::Store("inline preview fixture artifact missing".to_string())
        })?,
        external_blocked_artifact_id: external_blocked_artifact_id.ok_or_else(|| {
            DaemonError::Store("blocked URI fixture artifact missing".to_string())
        })?,
    })
}

#[derive(Debug)]
struct ManualSchedulerFixture {
    graph_id: RecursiveTaskGraphId,
    root_task_id: RecursiveTaskId,
}

fn create_manual_scheduler_target(
    store: &Store,
    ids: &DeterministicIds,
) -> Result<ManualSchedulerFixture> {
    create_graph(
        store,
        ids.manual_scheduler_graph_id,
        ids.manual_scheduler_root_task_id,
        "Smoke manual fake scheduler target",
        "Active fake graph reserved for RunRecursiveFakeScheduler max_steps=1 smoke",
    )?;
    Ok(ManualSchedulerFixture {
        graph_id: ids.manual_scheduler_graph_id,
        root_task_id: ids.manual_scheduler_root_task_id,
    })
}

#[derive(Debug)]
struct LiveDogfoodFixture {
    graph_id: RecursiveTaskGraphId,
}

fn create_live_dogfood_fixture(
    store: &Store,
    ids: &DeterministicIds,
) -> Result<LiveDogfoodFixture> {
    let detail = store.create_recursive_live_task_graph(RecursiveTaskGraphCreate {
        graph_id: ids.live_dogfood_graph_id,
        title: "Smoke ordinary live dogfood graph".to_string(),
        objective: "Launch one bounded live_session attempt from an isolated fixture DB"
            .to_string(),
        root_task: RecursiveRootTaskCreate {
            task_id: ids.live_dogfood_root_task_id,
            title: "Smoke ordinary live dogfood root".to_string(),
            objective: "Exercise uppercase L with explicit max_steps=1 after the operator enables the live scheduler gate".to_string(),
            scope: "dev-fixture-only ordinary live_session graph; no topology link and no provider session is launched by the fixture generator".to_string(),
            acceptance_criteria: vec![
                "fixture summary exposes this graph id for :dag selection".to_string(),
                "RunRecursiveLiveScheduler is invoked only after an explicit operator gate and max_steps".to_string(),
                "topology live delegation and background scheduling remain disabled".to_string(),
            ],
            scope_units: 8,
            max_retries: 0,
        },
        project_id: None,
        workflow_id: None,
        topology_id: None,
        parent_session_id: None,
        source_execution_id: None,
        source_eval_id: None,
        max_depth: 1,
        max_fanout: 1,
        max_descendants: 1,
        step_limit: 1,
    })?;
    if detail.graph.execution_mode != RecursiveExecutionMode::LiveSession {
        return Err(DaemonError::Store(format!(
            "live dogfood fixture graph {} was created with {:?}, expected {:?}",
            ids.live_dogfood_graph_id,
            detail.graph.execution_mode,
            RecursiveExecutionMode::LiveSession
        )));
    }
    if detail.graph.topology_id.is_some() {
        return Err(DaemonError::Store(
            "live dogfood fixture graph unexpectedly has topology linkage".to_string(),
        ));
    }
    Ok(LiveDogfoodFixture {
        graph_id: ids.live_dogfood_graph_id,
    })
}

#[derive(Debug)]
struct CancellableFixture {
    graph_id: RecursiveTaskGraphId,
    root_task_id: RecursiveTaskId,
    run_id: RecursiveSchedulerRunId,
}

fn create_cancellable_fixture(store: &Store, ids: &DeterministicIds) -> Result<CancellableFixture> {
    create_graph(
        store,
        ids.cancellable_graph_id,
        ids.cancellable_root_task_id,
        "Smoke cancellable fake graph",
        "Hold an active fake scheduler run for graph/run cancellation smoke",
    )?;
    let run = store.start_recursive_scheduler_run_with_lease_policy(
        RecursiveSchedulerRunStart {
            graph_id: ids.cancellable_graph_id,
            max_steps: 999,
            source: RecursiveSchedulerRunSource::TestHarness,
            operator: Some(FIXTURE_OPERATOR.to_string()),
            executor_mode: RecursiveExecutionMode::Fake,
        },
        RecursiveSchedulerLeasePolicy {
            lease_owner: FIXTURE_OPERATOR.to_string(),
            lease_ttl_seconds: CANCELLABLE_LEASE_TTL_SECONDS,
            max_active_runs: 16,
        },
    )?;
    Ok(CancellableFixture {
        graph_id: ids.cancellable_graph_id,
        root_task_id: ids.cancellable_root_task_id,
        run_id: run.id,
    })
}

#[derive(Debug)]
struct ValidationFixture {
    graph_id: RecursiveTaskGraphId,
    root_task_id: RecursiveTaskId,
    scheduler_run_id: RecursiveSchedulerRunId,
    attempt_id: RecursiveAttemptId,
    live_attempt_id: RecursiveLiveAttemptId,
    validation_id: RecursiveLiveOutputValidationId,
    raw_output_artifact_id: Option<i64>,
    validation_report_artifact_id: i64,
    produced_artifact_ids: Vec<i64>,
    test_artifact_ids: Vec<i64>,
    diff_artifact_ids: Vec<i64>,
    issue_count: u32,
    error_count: u32,
    warning_count: u32,
}

fn create_validation_fixture(store: &Store, ids: &DeterministicIds) -> Result<ValidationFixture> {
    create_graph(
        store,
        ids.validation_graph_id,
        ids.validation_root_task_id,
        "Smoke validation issue graph",
        "Persist validation warnings/errors without launching live execution",
    )?;
    let run = store.start_recursive_scheduler_run_with_lease_policy(
        RecursiveSchedulerRunStart {
            graph_id: ids.validation_graph_id,
            max_steps: 1,
            source: RecursiveSchedulerRunSource::TestHarness,
            operator: Some(FIXTURE_OPERATOR.to_string()),
            executor_mode: RecursiveExecutionMode::Fake,
        },
        RecursiveSchedulerLeasePolicy {
            lease_owner: FIXTURE_OPERATOR.to_string(),
            lease_ttl_seconds: 60,
            max_active_runs: 16,
        },
    )?;
    store.record_recursive_attempt_start(
        ids.validation_graph_id,
        ids.validation_root_task_id,
        RecursiveAttemptPhase::Execute,
        ids.validation_attempt_id,
    )?;
    store.transition_recursive_task_state(
        ids.validation_graph_id,
        ids.validation_root_task_id,
        RecursiveTaskLifecycleState::Running,
        Some("fixture-only validation attempt; no provider launched".to_string()),
    )?;
    let live = store.create_recursive_live_attempt_placeholder(RecursiveLiveAttemptCreate {
        id: ids.validation_live_attempt_id,
        graph_id: ids.validation_graph_id,
        task_id: ids.validation_root_task_id,
        scheduler_run_id: run.id,
        attempt_id: ids.validation_attempt_id,
        execution_mode: RecursiveExecutionMode::LiveSession,
        provider: None,
        model: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_worktree_id: None,
        workflow_execution_id: None,
        topology_workflow_id: None,
        max_wall_time_ms: None,
    })?;
    if live.summary.session_id.is_some() {
        return Err(DaemonError::Store(
            "validation fixture live attempt unexpectedly linked a session".to_string(),
        ));
    }

    let validation_result = validation_result(
        ids.validation_graph_id,
        ids.validation_root_task_id,
        run.id,
        ids.validation_attempt_id,
        ids.validation_live_attempt_id,
    );
    let issue_count = validation_result.summary.issue_count;
    let error_count = validation_result.summary.error_count;
    let warning_count = validation_result.summary.warning_count;
    let commit = RecursiveLiveOutputValidationCommit {
        live_attempt_id: ids.validation_live_attempt_id,
        graph_id: ids.validation_graph_id,
        task_id: ids.validation_root_task_id,
        scheduler_run_id: run.id,
        attempt_id: ids.validation_attempt_id,
        validated_output: None,
        validation_result,
        raw_output_artifact: Some(RecursiveExecutionArtifactCreate {
            task_id: ids.validation_root_task_id,
            attempt_id: Some(ids.validation_attempt_id),
            kind: RecursiveExecutionArtifactKind::Inline,
            label: "smoke-validation-raw-output".to_string(),
            content: Some(
                "{\"kind\":\"success\",\"missing\":\"required correlation\"}".to_string(),
            ),
            uri: None,
            metadata: fixture_metadata(json!({
                "artifact_role": "raw_output",
            })),
        }),
        produced_artifacts: vec![RecursiveExecutionArtifactCreate {
            task_id: ids.validation_root_task_id,
            attempt_id: Some(ids.validation_attempt_id),
            kind: RecursiveExecutionArtifactKind::Inline,
            label: "smoke-validation-produced-artifact".to_string(),
            content: Some("fixture-only produced artifact link".to_string()),
            uri: None,
            metadata: fixture_metadata(json!({
                "artifact_role": "produced_artifact",
            })),
        }],
        test_artifacts: vec![RecursiveExecutionArtifactCreate {
            task_id: ids.validation_root_task_id,
            attempt_id: Some(ids.validation_attempt_id),
            kind: RecursiveExecutionArtifactKind::Inline,
            label: "smoke-validation-test-summary".to_string(),
            content: Some("{\"status\":\"not_run\",\"fixture_only\":true}".to_string()),
            uri: None,
            metadata: fixture_metadata(json!({
                "artifact_role": "test_summary",
            })),
        }],
        diff_artifacts: vec![RecursiveExecutionArtifactCreate {
            task_id: ids.validation_root_task_id,
            attempt_id: Some(ids.validation_attempt_id),
            kind: RecursiveExecutionArtifactKind::Inline,
            label: "smoke-validation-diff-summary".to_string(),
            content: Some("{\"files\":[],\"fixture_only\":true}".to_string()),
            uri: None,
            metadata: fixture_metadata(json!({
                "artifact_role": "diff_summary",
            })),
        }],
        scheduler_event: None,
    };
    let committed = store.commit_recursive_live_output_validation(commit)?;
    store.finish_recursive_scheduler_run(
        run.id,
        RecursiveSchedulerStopReason::PartialFailure,
        0,
        None,
    )?;
    if committed.live_attempt.summary.status != RecursiveLiveAttemptStatus::Blocked {
        return Err(DaemonError::Store(format!(
            "validation fixture live attempt ended in {:?}, expected blocked",
            committed.live_attempt.summary.status
        )));
    }
    let validation_id = committed
        .validation_result
        .summary
        .validation_id
        .ok_or_else(|| DaemonError::Store("validation id was not assigned".to_string()))?;
    Ok(ValidationFixture {
        graph_id: ids.validation_graph_id,
        root_task_id: ids.validation_root_task_id,
        scheduler_run_id: run.id,
        attempt_id: ids.validation_attempt_id,
        live_attempt_id: ids.validation_live_attempt_id,
        validation_id,
        raw_output_artifact_id: committed.raw_output_artifact.map(|artifact| artifact.id),
        validation_report_artifact_id: committed.validation_artifact.id,
        produced_artifact_ids: committed
            .produced_artifacts
            .into_iter()
            .map(|artifact| artifact.id)
            .collect(),
        test_artifact_ids: committed
            .test_artifacts
            .into_iter()
            .map(|artifact| artifact.id)
            .collect(),
        diff_artifact_ids: committed
            .diff_artifacts
            .into_iter()
            .map(|artifact| artifact.id)
            .collect(),
        issue_count,
        error_count,
        warning_count,
    })
}

fn validation_result(
    graph_id: RecursiveTaskGraphId,
    task_id: RecursiveTaskId,
    scheduler_run_id: RecursiveSchedulerRunId,
    attempt_id: RecursiveAttemptId,
    live_attempt_id: RecursiveLiveAttemptId,
) -> RecursiveLiveOutputValidationResult {
    let issues = vec![
        RecursiveLiveOutputValidationIssue {
            code: RecursiveLiveValidationIssueCode::MissingRequiredField,
            severity: RecursiveLiveValidationIssueSeverity::Error,
            class: RecursiveLiveValidationIssueClass::Missing,
            location: Some(RecursiveLiveValidationIssueLocation::OutputPath {
                path: "/correlation".to_string(),
            }),
            message: "fixture-only validation error: missing live output correlation".to_string(),
            evidence: Vec::new(),
            suggested_next_action: Some(
                "inspect validation detail; do not launch live execution".to_string(),
            ),
            metadata: fixture_metadata(json!({
                "fixture_issue": "error",
            })),
        },
        RecursiveLiveOutputValidationIssue {
            code: RecursiveLiveValidationIssueCode::UnknownField,
            severity: RecursiveLiveValidationIssueSeverity::Warning,
            class: RecursiveLiveValidationIssueClass::Ambiguous,
            location: Some(RecursiveLiveValidationIssueLocation::OutputPath {
                path: "/unexpected_fixture_field".to_string(),
            }),
            message: "fixture-only validation warning: unexpected field retained for inspection"
                .to_string(),
            evidence: Vec::new(),
            suggested_next_action: None,
            metadata: fixture_metadata(json!({
                "fixture_issue": "warning",
            })),
        },
    ];
    RecursiveLiveOutputValidationResult {
        summary: RecursiveLiveOutputValidationSummary {
            validation_id: None,
            live_attempt_id,
            graph_id,
            task_id,
            scheduler_run_id,
            attempt_id,
            session_id: None,
            status: RecursiveLiveOutputValidationStatus::OperatorReviewRequired,
            output_kind: Some(RecursiveLiveOutputKind::Success),
            mapping_decision: Some(RecursiveLiveOutputMappingDecision::OperatorReview),
            retry_decision: Some(RecursiveLiveOutputRetryDecision {
                decision: RecursiveLiveOutputRetryDecisionKind::OperatorReview,
                reason: Some("fixture-only validation issue target".to_string()),
                remaining_repair_attempts: Some(0),
                remaining_task_retries: Some(0),
                suggested_next_action: Some("manual inspection only".to_string()),
            }),
            raw_output_artifact_id: None,
            normalized_output_artifact_id: None,
            validation_artifact_id: None,
            normalized_digest: None,
            issue_count: 2,
            error_count: 1,
            warning_count: 1,
            info_count: 0,
            created_at: None,
        },
        artifact_links: RecursiveLiveOutputValidationArtifactLinks::default(),
        parser_source: Some(RecursiveLiveOutputParserSource::Inline {
            description: Some("recursive DAG smoke fixture raw output".to_string()),
        }),
        issues,
        normalized_output: None,
        validation_report: Some(fixture_metadata(json!({
            "status": "operator_review_required",
            "live_execution_launched": false,
        }))),
        metadata: fixture_metadata(json!({
            "fixture_target": "validation_issue_readback",
        })),
    }
}

fn create_graph(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
    root_task_id: RecursiveTaskId,
    title: &str,
    objective: &str,
) -> Result<()> {
    store.create_recursive_task_graph(RecursiveTaskGraphCreate {
        graph_id,
        title: title.to_string(),
        objective: objective.to_string(),
        root_task: RecursiveRootTaskCreate {
            task_id: root_task_id,
            title: format!("{title} root"),
            objective: objective.to_string(),
            scope: "fixture-only recursive DAG smoke scope".to_string(),
            acceptance_criteria: vec![
                "readback is inspectable".to_string(),
                "no live execution is launched".to_string(),
            ],
            scope_units: 8,
            max_retries: 1,
        },
        project_id: None,
        workflow_id: None,
        topology_id: None,
        parent_session_id: None,
        source_execution_id: None,
        source_eval_id: None,
        max_depth: 4,
        max_fanout: 8,
        max_descendants: 32,
        step_limit: 16,
    })?;
    Ok(())
}

fn fixture_metadata(extra: serde_json::Value) -> serde_json::Value {
    let mut base = serde_json::Map::from_iter([
        (
            "fixture".to_string(),
            serde_json::Value::String(FIXTURE_SCHEMA_VERSION.to_string()),
        ),
        ("fixture_only".to_string(), serde_json::Value::Bool(true)),
        (
            "generator".to_string(),
            serde_json::Value::String("rsi-recursive-dag-smoke-fixture".to_string()),
        ),
        (
            "live_execution_launched".to_string(),
            serde_json::Value::Bool(false),
        ),
    ]);
    if let serde_json::Value::Object(extra) = extra {
        base.extend(extra);
    }
    serde_json::Value::Object(base)
}

fn deterministic_ids() -> DeterministicIds {
    DeterministicIds {
        topology_id: fixture_uuid("topology"),
        manual_scheduler_graph_id: RecursiveTaskGraphId(fixture_uuid("manual-scheduler-graph")),
        manual_scheduler_root_task_id: RecursiveTaskId(fixture_uuid("manual-scheduler-root")),
        live_dogfood_graph_id: RecursiveTaskGraphId(fixture_uuid("live-dogfood-graph")),
        live_dogfood_root_task_id: RecursiveTaskId(fixture_uuid("live-dogfood-root")),
        cancellable_graph_id: RecursiveTaskGraphId(fixture_uuid("cancellable-graph")),
        cancellable_root_task_id: RecursiveTaskId(fixture_uuid("cancellable-root")),
        recovery_a_graph_id: RecursiveTaskGraphId(fixture_uuid("recovery-a-graph")),
        recovery_a_root_task_id: RecursiveTaskId(fixture_uuid("recovery-a-root")),
        recovery_a_attempt_id: RecursiveAttemptId(fixture_uuid("recovery-a-attempt")),
        recovery_b_graph_id: RecursiveTaskGraphId(fixture_uuid("recovery-b-graph")),
        recovery_b_root_task_id: RecursiveTaskId(fixture_uuid("recovery-b-root")),
        recovery_b_attempt_id: RecursiveAttemptId(fixture_uuid("recovery-b-attempt")),
        quarantine_graph_id: RecursiveTaskGraphId(fixture_uuid("quarantine-graph")),
        quarantine_root_task_id: RecursiveTaskId(fixture_uuid("quarantine-root")),
        validation_graph_id: RecursiveTaskGraphId(fixture_uuid("validation-graph")),
        validation_root_task_id: RecursiveTaskId(fixture_uuid("validation-root")),
        validation_attempt_id: RecursiveAttemptId(fixture_uuid("validation-attempt")),
        validation_live_attempt_id: RecursiveLiveAttemptId(fixture_uuid("validation-live-attempt")),
    }
}

fn fixture_uuid(name: &str) -> Uuid {
    Uuid::new_v5(
        &FIXTURE_NAMESPACE,
        format!("{FIXTURE_SCHEMA_VERSION}:{name}").as_bytes(),
    )
}

fn write_summary_json(paths: &PreparedPaths, summary: &SmokeFixtureSummary) -> Result<()> {
    if let Some(parent) = paths.summary_json.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(summary)?;
    fs::write(&paths.summary_json, format!("{content}\n"))?;
    Ok(())
}

fn cleanup_command(paths: &PreparedPaths) -> String {
    if let Some(output_home) = paths.output_home.as_deref() {
        return format!("rm -rf {}", shell_quote(output_home));
    }
    format!(
        "rm -f {} {}",
        shell_quote(&paths.output_db),
        shell_quote(&paths.summary_json)
    )
}

fn shell_quote(path: &Path) -> String {
    let value = path.display().to_string();
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::store::recursive_dag::{
        RecursiveExecutionArtifactSummaryListFilter, RecursiveExecutionArtifactSummaryPageCursor,
        RecursiveExecutionArtifactSummaryPageOptions, RecursiveLiveAttemptListFilter,
        RecursiveLiveValidationIssueFilter,
    };
    use rsi_common::recursive_dag::RecursiveGraphStatus;
    use rusqlite::Connection;

    fn temp_options(dir: &tempfile::TempDir) -> SmokeFixtureOptions {
        SmokeFixtureOptions::from_output_db(
            dir.path().join("fixture.db"),
            dir.path().join("recursive-dag-smoke-fixture.json"),
        )
    }

    #[test]
    fn recursive_dag_smoke_fixture_refuses_default_db_path() {
        let Some(default_db) = default_home_db_path() else {
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let options =
            SmokeFixtureOptions::from_output_db(default_db, dir.path().join("summary.json"));
        let error = generate_smoke_fixture(options).expect_err("default DB must be refused");
        assert!(
            error
                .to_string()
                .contains("refusing to write fixture data to default production DB path"),
            "{error}"
        );
    }

    #[test]
    fn recursive_dag_smoke_fixture_refuses_existing_unmarked_output_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output_db = dir.path().join("fixture.db");
        std::fs::File::create(&output_db).expect("create unmarked db path");
        let options = SmokeFixtureOptions::from_output_db(
            output_db.clone(),
            dir.path().join("recursive-dag-smoke-fixture.json"),
        );

        let error = generate_smoke_fixture(options).expect_err("unmarked existing DB is refused");
        assert!(
            error
                .to_string()
                .contains("output DB already exists without fixture summary JSON"),
            "{error}"
        );
        assert_eq!(std::fs::metadata(output_db).expect("metadata").len(), 0);
    }

    #[test]
    fn recursive_dag_smoke_fixture_generates_store_readbacks_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = temp_options(&dir);
        let first = generate_smoke_fixture(options.clone()).expect("generate fixture");
        let second = generate_smoke_fixture(options).expect("rerun fixture");
        assert!(second.reused_existing);
        assert_eq!(first.ids, second.ids);
        assert_eq!(
            first.marker_artifact_id, second.marker_artifact_id,
            "rerun must not add a second marker"
        );
        assert!(first.ids.live_dogfood_graph_id.is_none());

        let store = Store::open(&dir.path().join("fixture.db")).expect("open fixture");
        assert!(
            store
                .get_recursive_task_graph(deterministic_ids().live_dogfood_graph_id)
                .expect("default live dogfood graph read")
                .is_none(),
            "ordinary live dogfood graph must be opt-in"
        );
        assert!(
            store
                .get_recursive_task_graph(first.ids.topology_graph_id)
                .expect("topology graph read")
                .is_some()
        );
        let topology_runs = store
            .list_recursive_scheduler_runs_for_graph(first.ids.topology_graph_id)
            .expect("topology runs");
        let fake_run = topology_runs
            .iter()
            .find(|run| run.id == first.ids.fake_scheduler_run_id)
            .expect("fake run exists");
        assert_eq!(fake_run.status, RecursiveSchedulerRunStatus::Completed);
        assert!(fake_run.report_artifact_id.is_some());
        assert!(
            !store
                .list_recursive_scheduler_run_events(first.ids.fake_scheduler_run_id, None, None)
                .expect("run events")
                .is_empty()
        );

        let page = store
            .list_recursive_execution_artifact_summaries(
                RecursiveExecutionArtifactSummaryListFilter {
                    graph_id: first.ids.topology_graph_id,
                    task_id: None,
                    attempt_id: None,
                    kind: None,
                },
                RecursiveExecutionArtifactSummaryPageOptions {
                    limit: 25,
                    after: None::<RecursiveExecutionArtifactSummaryPageCursor>,
                    include_total: true,
                },
            )
            .expect("artifact summaries");
        assert_eq!(page.items.len(), 25);
        assert!(page.has_more);
        assert!(page.total_count.expect("total") >= PAGINATION_ARTIFACT_COUNT as u64);
        require_artifact_preview_state(
            &store,
            first.ids.topology_graph_id,
            first.ids.inline_preview_artifact_id,
            RecursiveArtifactPreviewState::Available,
        )
        .expect("inline preview");
        require_artifact_preview_state(
            &store,
            first.ids.topology_graph_id,
            first.ids.external_blocked_artifact_id,
            RecursiveArtifactPreviewState::UriBlocked,
        )
        .expect("URI preview blocked");

        let validation = store
            .load_recursive_live_output_validation_result(
                first.ids.validation_id,
                RecursiveLiveOutputValidationReadOptions {
                    include_issues: true,
                    include_normalized_output: false,
                    include_validation_report: true,
                },
            )
            .expect("validation read")
            .expect("validation exists");
        assert_eq!(
            validation.summary.status,
            RecursiveLiveOutputValidationStatus::OperatorReviewRequired
        );
        assert_eq!(validation.summary.error_count, 1);
        assert_eq!(validation.summary.warning_count, 1);
        let issues = store
            .list_recursive_live_validation_issues(RecursiveLiveValidationIssueFilter {
                validation_id: Some(first.ids.validation_id),
                live_attempt_id: None,
                severity: None,
                class: None,
                code: None,
                limit: Some(10),
            })
            .expect("validation issues");
        assert_eq!(issues.len(), 2);

        let cancellable = store
            .load_recursive_scheduler_run(first.ids.cancellable_scheduler_run_id)
            .expect("cancellable run")
            .expect("cancellable run exists");
        assert_eq!(cancellable.status, RecursiveSchedulerRunStatus::Running);
        let deferred = store
            .list_deferred_recursive_recovery_graphs()
            .expect("deferred recovery");
        assert!(
            deferred
                .iter()
                .any(|graph| graph.graph_id == first.ids.deferred_recovery_graph_id)
        );
        let quarantined = store
            .get_recursive_task_graph(first.ids.quarantine_graph_id)
            .expect("quarantine graph")
            .expect("quarantine graph exists");
        assert_eq!(quarantined.graph.status, RecursiveGraphStatus::Malformed);

        let live_attempts = store
            .list_recursive_live_attempts(RecursiveLiveAttemptListFilter {
                graph_id: Some(first.ids.validation_graph_id),
                include_terminal: true,
                ..RecursiveLiveAttemptListFilter::default()
            })
            .expect("live attempts");
        assert_eq!(live_attempts.len(), 1);
        assert!(live_attempts[0].summary.session_id.is_none());
        assert_eq!(
            live_attempts[0].summary.status,
            RecursiveLiveAttemptStatus::Blocked
        );

        let conn = Connection::open(dir.path().join("fixture.db")).expect("open sqlite");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("session count");
        let launched_live_attempts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM recursive_live_attempts
                 WHERE session_id IS NOT NULL OR provider IS NOT NULL OR model IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("launched live attempt count");
        assert_eq!(sessions, 0);
        assert_eq!(launched_live_attempts, 0);
    }

    #[test]
    fn recursive_dag_smoke_fixture_can_opt_into_live_dogfood_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = temp_options(&dir).with_live_dogfood_graph();
        let first = generate_smoke_fixture(options.clone()).expect("generate fixture");
        let second = generate_smoke_fixture(options).expect("rerun fixture");
        assert!(second.reused_existing);
        assert_eq!(
            first.ids.live_dogfood_graph_id,
            second.ids.live_dogfood_graph_id
        );
        assert_eq!(
            first.marker_artifact_id, second.marker_artifact_id,
            "rerun must not add a second marker"
        );

        let live_graph_id = first
            .ids
            .live_dogfood_graph_id
            .expect("live dogfood graph id");
        let store = Store::open(&dir.path().join("fixture.db")).expect("open fixture");
        let graph = store
            .get_recursive_task_graph(live_graph_id)
            .expect("live graph read")
            .expect("live graph exists");
        assert_eq!(
            graph.graph.execution_mode,
            RecursiveExecutionMode::LiveSession
        );
        assert!(graph.graph.topology_id.is_none());
        assert_eq!(
            graph.graph.root_task_id,
            deterministic_ids().live_dogfood_root_task_id
        );
        let root = graph
            .nodes
            .iter()
            .find(|node| node.id == graph.graph.root_task_id)
            .expect("root task exists");
        assert_eq!(root.status, RecursiveTaskLifecycleState::Pending);
        assert_eq!(
            root.acceptance_criteria,
            vec![
                "fixture summary exposes this graph id for :dag selection".to_string(),
                "RunRecursiveLiveScheduler is invoked only after an explicit operator gate and max_steps".to_string(),
                "topology live delegation and background scheduling remain disabled".to_string(),
            ],
            "successful dogfood output must echo these exact criteria"
        );
        let live_attempts = store
            .list_recursive_live_attempts(RecursiveLiveAttemptListFilter {
                graph_id: Some(live_graph_id),
                include_terminal: true,
                ..RecursiveLiveAttemptListFilter::default()
            })
            .expect("live attempts");
        assert!(
            live_attempts.is_empty(),
            "fixture seeding must not launch or placeholder a live attempt for the dogfood graph"
        );
    }

    #[test]
    fn recursive_dag_smoke_fixture_can_upgrade_marked_fixture_with_live_dogfood_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = generate_smoke_fixture(temp_options(&dir)).expect("generate fixture");
        assert!(first.ids.live_dogfood_graph_id.is_none());

        let upgraded = generate_smoke_fixture(temp_options(&dir).with_live_dogfood_graph())
            .expect("upgrade fixture");
        assert!(upgraded.reused_existing);
        assert!(!upgraded.created_new_output_db);
        let live_graph_id = upgraded
            .ids
            .live_dogfood_graph_id
            .expect("live dogfood graph id");
        assert_eq!(live_graph_id, deterministic_ids().live_dogfood_graph_id);
        assert_ne!(
            first.marker_artifact_id, upgraded.marker_artifact_id,
            "upgrade should persist a refreshed fixture marker"
        );

        let store = Store::open(&dir.path().join("fixture.db")).expect("open fixture");
        require_live_dogfood_graph(&store, live_graph_id).expect("live graph valid");

        let sidecar = std::fs::read_to_string(dir.path().join("recursive-dag-smoke-fixture.json"))
            .expect("read sidecar");
        let sidecar_summary =
            serde_json::from_str::<SmokeFixtureSummary>(&sidecar).expect("parse sidecar");
        assert_eq!(
            sidecar_summary.ids.live_dogfood_graph_id,
            Some(live_graph_id)
        );

        let rerun = generate_smoke_fixture(temp_options(&dir).with_live_dogfood_graph())
            .expect("rerun upgraded fixture");
        assert!(rerun.reused_existing);
        assert_eq!(rerun.marker_artifact_id, upgraded.marker_artifact_id);
        assert_eq!(rerun.ids.live_dogfood_graph_id, Some(live_graph_id));
    }

    #[test]
    fn recursive_dag_smoke_fixture_copies_source_without_mutating_it() {
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let source_db = source_dir.path().join("source.db");
        Store::open(&source_db).expect("seed source schema");
        let before = sha256_file(&source_db).expect("source hash before");

        let output_dir = tempfile::tempdir().expect("output tempdir");
        let options = temp_options(&output_dir).with_source_db(source_db.clone());
        let summary = generate_smoke_fixture(options).expect("generate from source");
        let after = sha256_file(&source_db).expect("source hash after");
        assert_eq!(before, after);
        assert_eq!(
            summary.source_db_sha256_before.as_deref(),
            Some(before.as_str())
        );
        assert_eq!(
            summary.source_db_sha256_after.as_deref(),
            Some(after.as_str())
        );
        assert!(output_dir.path().join("fixture.db").exists());
    }

    #[test]
    fn recursive_dag_smoke_fixture_rehomes_copied_existing_fixture_summary() {
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let source = generate_smoke_fixture(temp_options(&source_dir)).expect("source fixture");
        let source_db = source_dir.path().join("fixture.db");

        let output_dir = tempfile::tempdir().expect("output tempdir");
        let output_summary_path = output_dir.path().join("recursive-dag-smoke-fixture.json");
        let copied = generate_smoke_fixture(temp_options(&output_dir).with_source_db(source_db))
            .expect("copy existing fixture source");

        assert!(copied.reused_existing);
        assert!(copied.created_new_output_db);
        assert_eq!(copied.ids, source.ids);
        assert_eq!(
            copied.output_db,
            output_dir.path().join("fixture.db").display().to_string()
        );
        assert_eq!(
            copied.summary_json,
            output_summary_path.display().to_string()
        );
        assert!(copied.source_db_sha256_before.is_some());
        assert_eq!(
            copied.source_db_sha256_before,
            copied.source_db_sha256_after
        );

        let sidecar = std::fs::read_to_string(&output_summary_path).expect("read copied sidecar");
        let sidecar_summary =
            serde_json::from_str::<SmokeFixtureSummary>(&sidecar).expect("parse sidecar");
        assert_eq!(sidecar_summary.output_db, copied.output_db);
        assert_eq!(sidecar_summary.summary_json, copied.summary_json);

        let rerun =
            generate_smoke_fixture(temp_options(&output_dir)).expect("rerun copied fixture output");
        assert!(rerun.reused_existing);
        assert!(!rerun.created_new_output_db);
        assert_eq!(rerun.ids, copied.ids);
    }
}
