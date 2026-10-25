//! One invocation owns the only mutable store handle for its project. The
//! manager serializes invocations, so no workspace can race another writer to
//! the same project SQLite file.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rsi_codegraph::{
    CodegraphError, CodegraphStore, ExtractionContract, ExtractionMode, ExtractorIdentity,
    PublishFault, ReadySnapshot, SourceVersion, StagedExtraction,
    cargo_metadata::CargoWorkspace,
    invalidate::ExtractionCache,
    lifecycle::{IndexLifecyclePhase, IndexPartialReason, IndexRunMetrics},
};

use super::{IndexError, RegisteredWorkspace, Result, discovery, retention};
use crate::bus::{DaemonEvent, EventBus};

const RUN_DEADLINE: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
pub struct WorkerState {
    cache: ExtractionCache,
    cache_versions: HashMap<String, String>,
    last_source_digest: Option<String>,
    last_config_digest: Option<String>,
    last_cargo_metadata_digest: Option<String>,
    #[cfg(test)]
    before_publish: Option<Box<dyn FnMut() + Send>>,
    #[cfg(test)]
    before_publish_commit: Option<Box<dyn FnMut() + Send>>,
}

/// #2464: the inputs of a workspace's last publication, which are all a pass
/// needs to recognise an unchanged workspace and confirm its ready head
/// instead of extracting and publishing an identical generation. A few
/// hundred bytes, so the manager keeps them for every workspace when it drops
/// the ~340 MB extraction caches (one resident at a time).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublishedDigests {
    source: Option<String>,
    config: Option<String>,
    cargo_metadata: Option<String>,
}

impl WorkerState {
    /// The digests of this state's last publication.
    #[must_use]
    pub fn published_digests(&self) -> PublishedDigests {
        PublishedDigests {
            source: self.last_source_digest.clone(),
            config: self.last_config_digest.clone(),
            cargo_metadata: self.last_cargo_metadata_digest.clone(),
        }
    }

    /// A state with no extraction cache that still knows its workspace's last
    /// publication: an unchanged workspace confirms its head, a changed one
    /// extracts in full exactly as a fresh state would.
    #[must_use]
    pub fn from_published_digests(digests: PublishedDigests) -> Self {
        Self {
            last_source_digest: digests.source,
            last_config_digest: digests.config,
            last_cargo_metadata_digest: digests.cargo_metadata,
            ..Self::default()
        }
    }
}

pub struct RunOutcome {
    pub ready: ReadySnapshot,
    pub files_discovered: usize,
    pub bytes_discovered: usize,
    pub files_hashed: usize,
    pub files_reused: usize,
    pub files_extracted: usize,
    pub changed_paths: usize,
    pub staged_files: usize,
    pub published: bool,
    pub skipped_symlinks: usize,
    pub partial_reasons: Vec<IndexPartialReason>,
}

/// Cargo facts for one run. A workspace whose locked dependencies cannot be
/// resolved offline degrades to its packages and targets, then to no Cargo
/// facts, and says so instead of failing the whole index.
struct CargoRead {
    workspace: Option<CargoWorkspace>,
    partial: Option<IndexPartialReason>,
}

impl CargoRead {
    fn read(root: &Path) -> Self {
        if !root.join("Cargo.toml").is_file() {
            return Self {
                workspace: None,
                partial: None,
            };
        }
        let resolved_error = match rsi_codegraph::cargo_metadata::read_workspace(root) {
            Ok(workspace) => {
                return Self {
                    workspace: Some(workspace),
                    partial: None,
                };
            }
            Err(error) => error,
        };
        match rsi_codegraph::cargo_metadata::read_workspace_no_deps(root) {
            Ok(workspace) => {
                tracing::debug!(%resolved_error, "cargo metadata unresolved; indexing workspace packages only");
                Self {
                    workspace: Some(workspace),
                    partial: Some(IndexPartialReason::CargoMetadataNoDeps),
                }
            }
            Err(no_deps_error) => {
                tracing::debug!(%resolved_error, %no_deps_error, "cargo metadata unavailable; indexing without Cargo facts");
                Self {
                    workspace: None,
                    partial: Some(IndexPartialReason::CargoMetadataUnavailable),
                }
            }
        }
    }

    /// The publish fence: re-read in the mode this run used and report whether
    /// the Cargo inputs still match. The unavailable mode has nothing to
    /// compare, and a retry of a known-failing read is not worth its wall time.
    fn unchanged(&self, root: &Path) -> Result<bool> {
        let current = match (&self.workspace, self.partial) {
            (None, _) => return Ok(true),
            (Some(_), None) => rsi_codegraph::cargo_metadata::read_workspace(root)?,
            (Some(_), Some(_)) => rsi_codegraph::cargo_metadata::read_workspace_no_deps(root)?,
        };
        Ok(self
            .workspace
            .as_ref()
            .is_some_and(|prior| prior.metadata_digest == current.metadata_digest))
    }

    fn digest(&self) -> Option<&str> {
        self.workspace
            .as_ref()
            .map(|workspace| workspace.metadata_digest.as_str())
    }
}

pub fn project_db_path(index_root: &Path, project_id: uuid::Uuid) -> std::path::PathBuf {
    index_root
        .join(project_id.to_string())
        .join("codegraph.sqlite")
}

/// An index pass whose resident size grew by at least this much is logged at
/// info with its corpus size, so the journal names the lane behind a start-up
/// memory spike (#3491).
const HEAVY_PASS_RSS_DELTA_BYTES: u64 = 64 * 1024 * 1024;

fn log_pass_memory(
    workspace: &RegisteredWorkspace,
    files: usize,
    source_bytes: usize,
    rss_start: Option<u64>,
    rss_after_update: Option<u64>,
    cache_retained: bool,
) {
    let (Some(start), Some(after)) = (rss_start, rss_after_update) else {
        return;
    };
    let grown = after.saturating_sub(start);
    let workspace_id = workspace.workspace_id;
    if grown >= HEAVY_PASS_RSS_DELTA_BYTES {
        tracing::info!(
            %workspace_id,
            files,
            source_bytes,
            rss_start_mb = start >> 20,
            rss_after_update_mb = after >> 20,
            rss_peak_mb = crate::process_memory::peak_rss_bytes().map_or(0, |peak| peak >> 20),
            cache_retained,
            "Codegraph index pass grew resident memory"
        );
    } else {
        tracing::debug!(
            %workspace_id,
            files,
            source_bytes,
            grown_mb = grown >> 20,
            cache_retained,
            "Codegraph index pass memory"
        );
    }
}

pub(super) fn extraction_manifest() -> StagedExtraction {
    let config_digest = blake3::hash(b"rsid-codegraph-discovery-policy-v1")
        .to_hex()
        .to_string();
    StagedExtraction {
        extraction: ExtractionContract {
            mode: ExtractionMode::ExtractedInferredV1_1,
            extractor: ExtractorIdentity {
                name: "rsi-codegraph".into(),
                version: "s2-v1".into(),
            },
        },
        grammar_version: "tree-sitter-s1".into(),
        rule_version: "resolve-module-tree-s2".into(),
        normalization_version: "s1".into(),
        config_digest,
    }
}

/// Discover, extract, stage the whole inventory, and publish only if this
/// request remains current and the exact source bytes still match.
#[cfg(test)]
pub fn index_once(
    state: &mut WorkerState,
    workspace: &RegisteredWorkspace,
    db_path: &Path,
    revision: &AtomicU64,
    observed_revision: u64,
) -> Result<RunOutcome> {
    index_once_with_events(
        state,
        workspace,
        db_path,
        revision,
        observed_revision,
        0,
        None,
    )
}

#[cfg(test)]
pub fn index_once_with_events(
    state: &mut WorkerState,
    workspace: &RegisteredWorkspace,
    db_path: &Path,
    revision: &AtomicU64,
    observed_revision: u64,
    overflow_count: u64,
    events: Option<&EventBus>,
) -> Result<RunOutcome> {
    index_once_with_ceiling(
        state,
        workspace,
        db_path,
        revision,
        observed_revision,
        overflow_count,
        events,
        retention::GLOBAL_DETAIL_CEILING_BYTES,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn index_once_with_events_and_gate(
    state: &mut WorkerState,
    workspace: &RegisteredWorkspace,
    db_path: &Path,
    revision: &AtomicU64,
    observed_revision: u64,
    overflow_count: u64,
    events: Option<&EventBus>,
    enabled: &AtomicBool,
) -> Result<RunOutcome> {
    let ceiling = db_path.parent().and_then(Path::parent).map_or(
        retention::GLOBAL_DETAIL_CEILING_BYTES,
        retention::effective_ceiling_bytes,
    );
    index_once_with_ceiling_and_gate(
        state,
        workspace,
        db_path,
        revision,
        observed_revision,
        overflow_count,
        events,
        ceiling,
        Some(enabled),
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn index_once_with_ceiling(
    state: &mut WorkerState,
    workspace: &RegisteredWorkspace,
    db_path: &Path,
    revision: &AtomicU64,
    observed_revision: u64,
    overflow_count: u64,
    events: Option<&EventBus>,
    ceiling: u64,
) -> Result<RunOutcome> {
    index_once_with_ceiling_and_gate(
        state,
        workspace,
        db_path,
        revision,
        observed_revision,
        overflow_count,
        events,
        ceiling,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn index_once_with_ceiling_and_gate(
    state: &mut WorkerState,
    workspace: &RegisteredWorkspace,
    db_path: &Path,
    revision: &AtomicU64,
    observed_revision: u64,
    overflow_count: u64,
    events: Option<&EventBus>,
    ceiling: u64,
    enabled: Option<&AtomicBool>,
) -> Result<RunOutcome> {
    if enabled.is_some_and(|enabled| !enabled.load(Ordering::Acquire)) {
        return Err(IndexError::Superseded);
    }
    let started = Instant::now();
    let parent = db_path
        .parent()
        .ok_or_else(|| IndexError::UnsafeWorkspace("index path has no parent".into()))?;
    let index_root = parent
        .parent()
        .ok_or_else(|| IndexError::UnsafeWorkspace("index path has no root".into()))?;
    std::fs::create_dir_all(parent)?;
    let mut store = CodegraphStore::open(db_path, workspace.project_id)?;
    store.recover_interrupted_index_runs()?;
    let scope = store.scope(workspace.instance.clone());
    if scope.workspace_id() != workspace.workspace_id {
        return Err(IndexError::UnsafeWorkspace(
            "workspace identity mismatch".into(),
        ));
    }
    let previous_ready = match store.current_ready(workspace.workspace_id) {
        Ok(ready) => Some(ready),
        Err(CodegraphError::NoReadySnapshot) => None,
        Err(error) => return Err(error.into()),
    };
    let discovery = match discovery::discover(workspace) {
        Ok(discovery) => discovery,
        Err(error) => {
            return preflight_failure(
                &mut store,
                workspace,
                previous_ready.is_some(),
                error,
                overflow_count,
                events,
            );
        }
    };
    if discovery.files.is_empty() {
        // S1 rejects empty staged inventories. Keeping the last ready head and
        // surfacing degraded state is safer than publishing an empty graph.
        return preflight_failure(
            &mut store,
            workspace,
            previous_ready.is_some(),
            IndexError::UnsafeWorkspace("no indexable source files".into()),
            overflow_count,
            events,
        );
    }
    let discovered_count = discovery.files.len();
    let manifest = extraction_manifest();
    let cargo = CargoRead::read(&workspace.root);
    let skipped_symlinks = discovery.skipped_symlinks.len();
    let partial_reasons = cargo.partial.into_iter().collect::<Vec<_>>();
    if let (Some(ready), Some(last_source), Some(last_config)) = (
        previous_ready.as_ref(),
        state.last_source_digest.as_deref(),
        state.last_config_digest.as_deref(),
    ) && last_source == discovery.digest
        && last_config == manifest.config_digest
        && state.last_cargo_metadata_digest.as_deref() == cargo.digest()
    {
        if store.confirm_index_ready(workspace.workspace_id, &discovery.digest, overflow_count)? {
            publish_status_hint(&store, workspace, events);
        }
        return Ok(RunOutcome {
            ready: ready.clone(),
            files_discovered: discovered_count,
            bytes_discovered: discovery.bytes,
            files_hashed: discovered_count,
            files_reused: discovered_count,
            files_extracted: 0,
            changed_paths: 0,
            staged_files: 0,
            published: false,
            skipped_symlinks,
            partial_reasons,
        });
    }
    if let Err(error) = retention::admit_write_with_ceiling(index_root, db_path, &store, ceiling) {
        return preflight_failure(
            &mut store,
            workspace,
            previous_ready.is_some(),
            error,
            overflow_count,
            events,
        );
    }
    let run_id = store.begin_index_run(workspace.workspace_id, &discovery.digest, &manifest)?;
    publish_status_hint(&store, workspace, events);
    let mut reused_files = 0usize;
    let mut extracted_files = 0usize;
    let rss_start = crate::process_memory::rss_bytes();
    let mut rss_after_update = None;
    let result = (|| -> Result<(RunOutcome, Vec<SourceVersion>)> {
        // A corpus over the cache's retention budget streams: its facts spill to
        // an unlinked file beside the index and flow back one file at a time into
        // staging, so the pass holds the resolver's inputs, not the corpus (#3491).
        let mut update =
            state
                .cache
                .update_pass(discovery.files, cargo.workspace.as_ref(), parent)?;
        rss_after_update = crate::process_memory::rss_bytes();
        let owners = update
            .facts
            .source_digests()
            .into_iter()
            .map(|(relative_path, digest)| {
                if state.cache_versions.get(&relative_path) == Some(&digest) {
                    reused_files += 1;
                }
                SourceVersion {
                    relative_path,
                    source_digest: digest,
                }
            })
            .collect::<Vec<_>>();
        extracted_files = owners.len().saturating_sub(reused_files);
        state.cache_versions = owners
            .iter()
            .map(|owner| (owner.relative_path.clone(), owner.source_digest.clone()))
            .collect();
        let changed_paths = update.changed_owners.len();
        loop {
            if revision.load(Ordering::Acquire) != observed_revision {
                return Err(IndexError::Superseded);
            }
            if started.elapsed() >= RUN_DEADLINE {
                return Err(IndexError::DiscoveryLimit("run deadline"));
            }
            let run = store.begin_staged(&scope, &manifest, &owners)?;
            update.facts.for_each(|facts| {
                if revision.load(Ordering::Acquire) != observed_revision {
                    return Err(IndexError::Superseded);
                }
                store.stage_file(&run, facts)?;
                Ok::<(), IndexError>(())
            })?;
            // Filesystem notifications can be lost; the second exact-byte scan
            // prevents publishing a manifest that changed during extraction.
            if discovery::discover_digest(workspace)?.digest != discovery.digest {
                return Err(IndexError::SourceChanged);
            }
            if !cargo.unchanged(&workspace.root)? {
                return Err(IndexError::SourceChanged);
            }
            if revision.load(Ordering::Acquire) != observed_revision {
                return Err(IndexError::Superseded);
            }
            #[cfg(test)]
            if let Some(hook) = state.before_publish.as_mut() {
                hook();
            }
            if enabled.is_some_and(|enabled| !enabled.load(Ordering::Acquire)) {
                return Err(IndexError::Superseded);
            }
            retention::admit_write_with_ceiling(index_root, db_path, &store, ceiling)?;
            let publication_guard = enabled.map(|_| super::PUBLICATION_GATE.blocking_lock());
            if enabled.is_some_and(|enabled| !enabled.load(Ordering::Acquire)) {
                return Err(IndexError::Superseded);
            }
            #[cfg(test)]
            if let Some(hook) = state.before_publish_commit.as_mut() {
                hook();
            }
            match store.publish_staged(&run, PublishFault::None) {
                Ok(ready) => {
                    store.record_published_generation(workspace.workspace_id, ready.generation)?;
                    state.last_source_digest = Some(discovery.digest);
                    state.last_config_digest = Some(manifest.config_digest);
                    state.last_cargo_metadata_digest =
                        cargo.workspace.map(|metadata| metadata.metadata_digest);
                    return Ok((
                        RunOutcome {
                            ready,
                            files_discovered: owners.len(),
                            bytes_discovered: discovery.bytes,
                            files_hashed: owners.len(),
                            files_reused: reused_files,
                            files_extracted: extracted_files,
                            changed_paths,
                            staged_files: owners.len(),
                            published: true,
                            skipped_symlinks,
                            partial_reasons: partial_reasons.clone(),
                        },
                        owners,
                    ));
                }
                Err(CodegraphError::RetryableWriterConflict)
                    if started.elapsed() < RUN_DEADLINE =>
                {
                    drop(publication_guard);
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error.into()),
            }
        }
    })()
    .map_err(|error| retention::typed_budget_failure(error, index_root, ceiling));
    log_pass_memory(
        workspace,
        discovered_count,
        discovery.bytes,
        rss_start,
        rss_after_update,
        state.cache.retains_raw_facts(),
    );
    let metrics = match &result {
        Ok((outcome, _)) => IndexRunMetrics {
            files_discovered: outcome.files_discovered,
            bytes_discovered: outcome.bytes_discovered,
            files_hashed: outcome.files_hashed,
            files_reused: outcome.files_reused,
            files_extracted: outcome.files_extracted,
            changed_paths: outcome.changed_paths,
            staged_files: outcome.staged_files,
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            overflow_count,
            pending_rescan: revision.load(Ordering::Acquire) != observed_revision,
            rescan_reason: (revision.load(Ordering::Acquire) != observed_revision)
                .then(|| "revision_changed".into()),
            skipped_symlinks: outcome.skipped_symlinks,
            partial_reasons: outcome.partial_reasons.clone(),
        },
        Err(_) => IndexRunMetrics {
            files_discovered: discovered_count,
            bytes_discovered: discovery.bytes,
            files_hashed: discovered_count,
            files_reused: reused_files,
            files_extracted: extracted_files,
            changed_paths: 0,
            staged_files: 0,
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            overflow_count,
            pending_rescan: true,
            rescan_reason: Some("run_failed".into()),
            skipped_symlinks,
            partial_reasons: partial_reasons.clone(),
        },
    };
    match &result {
        Ok((_, owners)) => store.finish_index_run(
            run_id,
            workspace.workspace_id,
            if metrics.pending_rescan {
                IndexLifecyclePhase::Stale
            } else {
                IndexLifecyclePhase::Ready
            },
            &metrics,
            Some(owners),
            None,
        )?,
        Err(error) => store.finish_index_run(
            run_id,
            workspace.workspace_id,
            if previous_ready.is_some() {
                IndexLifecyclePhase::Degraded
            } else {
                IndexLifecyclePhase::Failed
            },
            &metrics,
            None,
            Some(&error.to_string()),
        )?,
    }
    publish_status_hint(&store, workspace, events);
    if result.as_ref().is_ok_and(|(outcome, _)| outcome.published) {
        match retention::enforce(index_root) {
            Ok(plan)
                if plan.protected_over_ceiling_bytes > 0
                    || plan.physical_over_ceiling_bytes > 0 =>
            {
                tracing::warn!(
                    protected_logical_bytes = plan.protected_over_ceiling_bytes,
                    physical_bytes_over_ceiling = plan.physical_over_ceiling_bytes,
                    "Codegraph retention exceeds the detail disk ceiling"
                );
            }
            Ok(_) => {}
            Err(error) => {
                store.mark_index_degraded(workspace.workspace_id, &error.to_string())?;
                publish_status_hint(&store, workspace, events);
                return Err(error);
            }
        }
    }
    result.map(|(outcome, _)| outcome)
}

fn preflight_failure<T>(
    store: &mut CodegraphStore,
    workspace: &RegisteredWorkspace,
    had_ready: bool,
    error: IndexError,
    overflow_count: u64,
    events: Option<&EventBus>,
) -> Result<T> {
    store.record_index_preflight_failure(
        workspace.workspace_id,
        had_ready,
        &error.to_string(),
        overflow_count,
    )?;
    publish_status_hint(store, workspace, events);
    Err(error)
}

fn publish_status_hint(
    store: &CodegraphStore,
    workspace: &RegisteredWorkspace,
    events: Option<&EventBus>,
) {
    if let Some(events) = events
        && let Ok(Some(status)) = store.index_status(workspace.workspace_id)
    {
        events.publish(DaemonEvent::CodegraphIndexStatus {
            project_id: workspace.project_id,
            workspace_id: workspace.workspace_id,
            phase: status.phase,
            generation: status.ready.map(|ready| ready.generation),
            run_id: status.run_id,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    fn assert_ready_facts_equal(
        incremental_db: &std::path::Path,
        incremental: &rsi_codegraph::ReadySnapshot,
        clean_db: &std::path::Path,
        rebuilt: &rsi_codegraph::ReadySnapshot,
    ) {
        let incremental_connection = rusqlite::Connection::open(incremental_db).unwrap();
        let clean_connection = rusqlite::Connection::open(clean_db).unwrap();
        for table in [
            "cg_files",
            "cg_nodes",
            "cg_relations",
            "cg_evidence",
            "cg_unresolved_references",
            "cg_file_diagnostics",
        ] {
            let rows = |connection: &rusqlite::Connection, ready: &rsi_codegraph::ReadySnapshot| {
                let mut statement = connection
                    .prepare(&format!(
                        "SELECT * FROM {table} WHERE workspace_id = ?1 AND generation = ?2"
                    ))
                    .unwrap();
                let column_count = statement.column_count();
                let mut rows = statement
                    .query_map(
                        rusqlite::params![ready.workspace_id.to_string(), ready.generation],
                        |row| {
                            (2..column_count)
                                .map(|column| {
                                    row.get::<_, rusqlite::types::Value>(column)
                                        .map(|value| format!("{value:?}"))
                                })
                                .collect::<rusqlite::Result<Vec<_>>>()
                        },
                    )
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                rows.sort();
                rows
            };
            assert_eq!(
                rows(&incremental_connection, incremental),
                rows(&clean_connection, rebuilt),
                "current-ready rows differ in {table}"
            );
        }
    }

    /// #2464: the manager keeps one extraction cache resident, so a workspace's
    /// next pass often starts from its published digests alone. An unchanged
    /// tree then confirms its ready head (no extraction, no new generation for
    /// retention to prune); a changed tree still extracts and publishes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn unchanged_workspace_confirms_its_head_from_published_digests_2464() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let mut resident = WorkerState::default();
        let first = index_once(&mut resident, &workspace, &db, &revision, 1).unwrap();
        assert!(first.published);
        let mut evicted = WorkerState::from_published_digests(resident.published_digests());
        drop(resident);

        let unchanged = index_once(&mut evicted, &workspace, &db, &revision, 1).unwrap();
        assert!(
            !unchanged.published,
            "identical input publishes no generation"
        );
        assert_eq!(unchanged.files_extracted, 0);
        assert_eq!(unchanged.ready, first.ready);

        std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
        let changed = index_once(&mut evicted, &workspace, &db, &revision, 1).unwrap();
        assert!(changed.published);
        assert!(changed.ready.generation > first.ready.generation);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn disabling_during_staging_prevents_publish_and_preserves_ready_head() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn first() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let mut state = WorkerState::default();
        let ready = index_once(&mut state, &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        let manifest = CodegraphStore::open(&db, workspace.project_id)
            .unwrap()
            .index_manifest(workspace.workspace_id)
            .unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn second() {}\n").unwrap();
        let enabled = Arc::new(AtomicBool::new(true));
        let disable_at_publish = enabled.clone();
        state.before_publish = Some(Box::new(move || {
            disable_at_publish.store(false, Ordering::Release);
        }));

        assert!(matches!(
            index_once_with_events_and_gate(
                &mut state, &workspace, &db, &revision, 1, 0, None, &enabled,
            ),
            Err(IndexError::Superseded)
        ));
        assert!(!enabled.load(Ordering::Acquire));
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        assert_eq!(store.current_ready(workspace.workspace_id).unwrap(), ready);
        assert_eq!(
            store.index_manifest(workspace.workspace_id).unwrap(),
            manifest
        );
        assert_eq!(
            store
                .index_status(workspace.workspace_id)
                .unwrap()
                .unwrap()
                .phase,
            IndexLifecyclePhase::Degraded
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn acknowledged_disable_cannot_precede_a_ready_head_promotion() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn first() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = Arc::new(AtomicU64::new(1));
        let mut state = WorkerState::default();
        index_once(&mut state, &workspace, &db, &revision, 1).unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn second() {}\n").unwrap();

        let (at_boundary_tx, at_boundary_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        state.before_publish_commit = Some(Box::new(move || {
            at_boundary_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        let enabled = Arc::new(AtomicBool::new(true));
        let worker_enabled = Arc::clone(&enabled);
        let worker_revision = Arc::clone(&revision);
        let worker_workspace = workspace.clone();
        let worker_db = db.clone();
        let worker = std::thread::spawn(move || {
            index_once_with_events_and_gate(
                &mut state,
                &worker_workspace,
                &worker_db,
                &worker_revision,
                1,
                0,
                None,
                &worker_enabled,
            )
        });
        at_boundary_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let disable_enabled = Arc::clone(&enabled);
        let disable_db = db.clone();
        let disable_workspace = workspace.clone();
        let disable = std::thread::spawn(move || {
            let _publication_guard = super::super::PUBLICATION_GATE.blocking_lock();
            disable_enabled.store(false, Ordering::Release);
            let at_ack = CodegraphStore::open(&disable_db, disable_workspace.project_id)
                .unwrap()
                .current_ready(disable_workspace.workspace_id)
                .unwrap();
            ack_tx.send(at_ack).unwrap();
        });
        let early_ack = ack_rx.recv_timeout(Duration::from_millis(150)).ok();
        release_tx.send(()).unwrap();
        let outcome = worker.join().unwrap().unwrap();
        let ready_at_ack =
            early_ack.unwrap_or_else(|| ack_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        disable.join().unwrap();
        assert!(outcome.published);
        assert_eq!(
            ready_at_ack,
            CodegraphStore::open(&db, workspace.project_id)
                .unwrap()
                .current_ready(workspace.workspace_id)
                .unwrap(),
            "ready head changed after disable acknowledged"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn exhausted_physical_budget_preserves_ready_head_and_manifest() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let mut state = WorkerState::default();
        let ready = index_once(&mut state, &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        let manifest = CodegraphStore::open(&db, workspace.project_id)
            .unwrap()
            .index_manifest(workspace.workspace_id)
            .unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
        assert!(matches!(
            index_once_with_ceiling(&mut state, &workspace, &db, &revision, 1, 0, None, 1),
            Err(IndexError::DiskBudget { .. })
        ));
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        assert_eq!(store.current_ready(workspace.workspace_id).unwrap(), ready);
        assert_eq!(
            store.index_manifest(workspace.workspace_id).unwrap(),
            manifest
        );
        let status = store.index_status(workspace.workspace_id).unwrap().unwrap();
        assert_eq!(status.phase, IndexLifecyclePhase::Degraded);
        assert!(status.metrics.pending_rescan);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn pinned_wal_reader_defers_small_budget_write_without_replacing_ready_head() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let mut state = WorkerState::default();
        let ready = index_once(&mut state, &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        let manifest = CodegraphStore::open(&db, workspace.project_id)
            .unwrap()
            .index_manifest(workspace.workspace_id)
            .unwrap();

        let reader = rusqlite::Connection::open(&db).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let observed: i64 = reader
            .query_row("SELECT COUNT(*) FROM cg_index_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(observed, 1);
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer
            .execute_batch("CREATE TABLE cg_test_pinned_reader (id INTEGER)")
            .unwrap();
        drop(writer);
        assert!(
            std::fs::metadata(format!("{}-wal", db.display()))
                .unwrap()
                .len()
                > 0
        );

        let physical = ["", "-wal", "-shm"]
            .into_iter()
            .filter_map(|suffix| std::fs::metadata(format!("{}{suffix}", db.display())).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>();
        let ceiling = physical.saturating_mul(4);
        assert!(physical < ceiling);
        std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
        assert!(matches!(
            index_once_with_ceiling(&mut state, &workspace, &db, &revision, 1, 0, None, ceiling),
            Err(IndexError::ActiveReader)
        ));
        let after_rejection = ["", "-wal", "-shm"]
            .into_iter()
            .filter_map(|suffix| std::fs::metadata(format!("{}{suffix}", db.display())).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>();
        assert!(after_rejection <= ceiling);
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        assert_eq!(store.current_ready(workspace.workspace_id).unwrap(), ready);
        assert_eq!(
            store.index_manifest(workspace.workspace_id).unwrap(),
            manifest
        );
        assert_eq!(
            store
                .index_status(workspace.workspace_id)
                .unwrap()
                .unwrap()
                .phase,
            IndexLifecyclePhase::Degraded
        );
        drop(store);

        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        let successor =
            index_once_with_ceiling(&mut state, &workspace, &db, &revision, 1, 0, None, ceiling)
                .unwrap();
        assert!(successor.published);
        assert_ne!(successor.ready, ready);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn durable_run_manifest_survives_failed_successor() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let bytes = b"pub fn ready() {}\n";
        std::fs::write(root.path().join("lib.rs"), bytes).unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let ready = index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        assert_eq!(
            store
                .index_status(workspace.workspace_id)
                .unwrap()
                .unwrap()
                .phase,
            IndexLifecyclePhase::Ready
        );
        let original_manifest = store.index_manifest(workspace.workspace_id).unwrap();
        assert_eq!(original_manifest.len(), 1);
        assert_eq!(original_manifest[0].relative_path, "lib.rs");
        assert_eq!(
            original_manifest[0].source_digest,
            blake3::hash(bytes).to_hex().to_string()
        );
        drop(store);
        std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
        revision.store(2, Ordering::Release);
        assert!(matches!(
            index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1),
            Err(IndexError::Superseded)
        ));
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        let status = store.index_status(workspace.workspace_id).unwrap().unwrap();
        assert_eq!(status.phase, IndexLifecyclePhase::Degraded);
        assert_eq!(status.ready.unwrap().snapshot_digest, ready.snapshot_digest);
        assert_eq!(
            store.index_manifest(workspace.workspace_id).unwrap(),
            original_manifest
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn pruning_preserves_ready_head_and_linked_identity() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let mut state = WorkerState::default();
        std::fs::write(root.path().join("lib.rs"), "pub fn first() {}\n").unwrap();
        let first = index_once(&mut state, &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        std::fs::write(root.path().join("lib.rs"), "pub fn second() {}\n").unwrap();
        let second = index_once(&mut state, &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        let mut store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        let details = store.generation_details().unwrap();
        assert_eq!(details.len(), 2);
        assert!(details.iter().all(|detail| detail.detailed_bytes > 0));
        assert_eq!(details.iter().filter(|detail| detail.current).count(), 1);
        drop(store);
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute(
            "UPDATE cg_generation_detail SET published_at=NULL,detailed_bytes=0 WHERE workspace_id=?1 AND generation=?2",
            rusqlite::params![workspace.workspace_id.to_string(),first.generation],
        ).unwrap();
        drop(connection);
        let mut store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        let legacy = store.generation_details().unwrap();
        assert!(
            legacy
                .iter()
                .find(|row| row.generation == first.generation)
                .is_some_and(|row| row.detailed_bytes > 0 && !row.published_at.is_empty())
        );
        store
            .prune_detailed_generation(workspace.workspace_id, first.generation)
            .unwrap();
        assert_eq!(store.current_ready(workspace.workspace_id).unwrap(), second);
        assert_eq!(store.generation_details().unwrap().len(), 1);
        drop(store);
        let connection = rusqlite::Connection::open(&db).unwrap();
        let identity: (String, i64) = connection.query_row(
            "SELECT name,tombstoned FROM cg_retained_nodes WHERE workspace_id=?1 AND name='first'",
            [workspace.workspace_id.to_string()], |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        assert_eq!(identity, ("first".into(), 1));
        let violations: i64 = connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn empty_inventory_is_durable_failure_then_degraded_with_ready_head() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        assert!(index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1).is_err());
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        assert_eq!(
            store
                .index_status(workspace.workspace_id)
                .unwrap()
                .unwrap()
                .phase,
            IndexLifecyclePhase::Failed
        );
        drop(store);
        std::fs::write(root.path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
        let ready = index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        std::fs::remove_file(root.path().join("lib.rs")).unwrap();
        assert!(index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1).is_err());
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        let status = store.index_status(workspace.workspace_id).unwrap().unwrap();
        assert_eq!(status.phase, IndexLifecyclePhase::Degraded);
        assert_eq!(status.ready.unwrap(), ready);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn restored_identical_source_clears_durable_degraded_status() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        let source = b"pub fn ready() {}\n";
        std::fs::write(root.path().join("lib.rs"), source).unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let mut state = WorkerState::default();
        let published = index_once(&mut state, &workspace, &db, &revision, 1).unwrap();
        std::fs::remove_file(root.path().join("lib.rs")).unwrap();
        assert!(index_once(&mut state, &workspace, &db, &revision, 1).is_err());
        std::fs::write(root.path().join("lib.rs"), source).unwrap();
        let unchanged = index_once(&mut state, &workspace, &db, &revision, 1).unwrap();
        assert!(!unchanged.published);
        assert_eq!(unchanged.ready, published.ready);
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        assert_eq!(
            store
                .index_status(workspace.workspace_id)
                .unwrap()
                .unwrap()
                .phase,
            IndexLifecyclePhase::Ready
        );
        assert_eq!(
            store.index_manifest(workspace.workspace_id).unwrap().len(),
            1
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn bus_hints_follow_durable_building_and_ready_transitions() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let bus = EventBus::new(8);
        let mut receiver = bus.subscribe();
        index_once_with_events(
            &mut WorkerState::default(),
            &workspace,
            &db,
            &AtomicU64::new(1),
            1,
            7,
            Some(&bus),
        )
        .unwrap();
        let building = receiver.try_recv().unwrap();
        let ready = receiver.try_recv().unwrap();
        assert!(matches!(
            &*building,
            DaemonEvent::CodegraphIndexStatus {
                phase: IndexLifecyclePhase::Building,
                ..
            }
        ));
        assert!(matches!(
            &*ready,
            DaemonEvent::CodegraphIndexStatus {
                phase: IndexLifecyclePhase::Ready,
                generation: Some(1),
                ..
            }
        ));
        let converted: rsi_common::rpc::BusEvent = (*ready).clone().into();
        assert_eq!(converted.event_type, "codegraph.index.status");
        let status = CodegraphStore::open(&db, workspace.project_id)
            .unwrap()
            .index_status(workspace.workspace_id)
            .unwrap()
            .unwrap();
        assert_eq!(status.phase, IndexLifecyclePhase::Ready);
        assert_eq!(status.metrics.overflow_count, 7);
    }

    /// #3491: a corpus over the extraction cache's retention budget is streamed
    /// (its facts spill and flow back into staging one file at a time); it must
    /// publish exactly the rows the in-memory pass publishes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn a_streamed_pass_publishes_the_same_rows_as_the_in_memory_pass() {
        let root = tempfile::tempdir().unwrap();
        let memory_dir = tempfile::tempdir().unwrap();
        let streamed_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("lib.rs"),
            "mod util;\nuse util::helper;\npub fn caller() { helper(); util::helper(); missing(); }\n",
        )
        .unwrap();
        std::fs::write(root.path().join("util.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(root.path().join("plan.md"), "# Plan\nsatisfies: [F-001]\n").unwrap();
        std::fs::write(
            root.path().join("findings.md"),
            "# Findings\n## F-001 Evidence\n",
        )
        .unwrap();
        let revision = AtomicU64::new(1);

        // One workspace identity, two index roots: row ids derive from it.
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let memory_workspace = workspace.clone();
        let memory_db = project_db_path(memory_dir.path(), memory_workspace.project_id);
        let mut memory = WorkerState::default();
        let in_memory =
            index_once(&mut memory, &memory_workspace, &memory_db, &revision, 1).unwrap();
        assert!(in_memory.published);

        let streamed_workspace = workspace;
        let streamed_db = project_db_path(streamed_dir.path(), streamed_workspace.project_id);
        let mut streamed = WorkerState {
            cache: ExtractionCache::with_retain_budget(0),
            ..WorkerState::default()
        };
        let outcome = index_once(
            &mut streamed,
            &streamed_workspace,
            &streamed_db,
            &revision,
            1,
        )
        .unwrap();
        assert!(outcome.published);
        assert_eq!(outcome.files_discovered, in_memory.files_discovered);
        assert!(
            !streamed.cache.retains_raw_facts(),
            "a streamed pass keeps digests only"
        );
        assert_ready_facts_equal(&streamed_db, &outcome.ready, &memory_db, &in_memory.ready);

        // The streamed state still recognises an unchanged tree and a change.
        let unchanged = index_once(
            &mut streamed,
            &streamed_workspace,
            &streamed_db,
            &revision,
            1,
        )
        .unwrap();
        assert!(!unchanged.published);
        std::fs::write(
            root.path().join("util.rs"),
            "pub fn helper() { }\npub fn more() {}\n",
        )
        .unwrap();
        let changed = index_once(
            &mut streamed,
            &streamed_workspace,
            &streamed_db,
            &revision,
            1,
        )
        .unwrap();
        assert!(changed.published);
        assert_eq!(changed.changed_paths, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn edits_and_deletes_publish_complete_inventory() {
        let root = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.rs"), "pub fn b() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(db_dir.path(), workspace.project_id);
        let mut state = WorkerState::default();
        let revision = AtomicU64::new(1);
        let first = index_once(&mut state, &workspace, &db, &revision, 1).unwrap();
        assert_eq!(first.files_discovered, 2);
        assert!(first.published);
        std::fs::remove_file(root.path().join("b.rs")).unwrap();
        let second = index_once(&mut state, &workspace, &db, &revision, 1).unwrap();
        assert_eq!(second.files_discovered, 1);
        assert_eq!(second.ready.generation, first.ready.generation + 1);
        let reopened = CodegraphStore::open(&db, workspace.project_id).unwrap();
        assert_eq!(
            reopened
                .current_completeness(workspace.workspace_id)
                .unwrap()
                .total_files,
            1
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn cold_restart_rebuilds_and_preserves_last_ready_on_failure() {
        let root = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn old() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(db_dir.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let old = index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        revision.store(2, Ordering::Release);
        assert!(matches!(
            index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1),
            Err(IndexError::Superseded)
        ));
        let retained = CodegraphStore::open(&db, workspace.project_id)
            .unwrap()
            .current_ready(workspace.workspace_id)
            .unwrap();
        assert_eq!(retained.snapshot_digest, old.snapshot_digest);
        std::fs::write(root.path().join("lib.rs"), "pub fn new() {}\n").unwrap();
        let recovered = index_once(&mut WorkerState::default(), &workspace, &db, &revision, 2)
            .unwrap()
            .ready;
        assert_ne!(recovered.snapshot_digest, old.snapshot_digest);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn cold_restart_replaces_unpublished_stage_and_keeps_ready_head() {
        let root = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn old() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(db_dir.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let old = index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        let new_bytes = b"pub fn new() {}\n";
        std::fs::write(root.path().join("lib.rs"), new_bytes).unwrap();
        {
            let mut store = CodegraphStore::open(&db, workspace.project_id).unwrap();
            let scope = store.scope(workspace.instance.clone());
            let owners = vec![SourceVersion {
                relative_path: "lib.rs".into(),
                source_digest: blake3::hash(new_bytes).to_hex().to_string(),
            }];
            let _interrupted = store
                .begin_staged(&scope, &extraction_manifest(), &owners)
                .unwrap();
        }
        let retained = CodegraphStore::open(&db, workspace.project_id)
            .unwrap()
            .current_ready(workspace.workspace_id)
            .unwrap();
        assert_eq!(retained.snapshot_digest, old.snapshot_digest);
        let recovered = index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1)
            .unwrap()
            .ready;
        assert_ne!(recovered.snapshot_digest, old.snapshot_digest);
        assert_eq!(recovered.generation, old.generation + 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn incremental_mutations_match_cold_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let incremental_dir = tempfile::tempdir().unwrap();
        let project = uuid::Uuid::new_v4();
        let workspace = RegisteredWorkspace::primary(project, root.path()).unwrap();
        let incremental_db = project_db_path(incremental_dir.path(), project);
        let mut cache = WorkerState::default();
        let revision = AtomicU64::new(1);
        let mut compare = || {
            let incremental = index_once(&mut cache, &workspace, &incremental_db, &revision, 1)
                .unwrap()
                .ready;
            let clean_dir = tempfile::tempdir().unwrap();
            let clean_db = project_db_path(clean_dir.path(), project);
            let rebuilt = index_once(
                &mut WorkerState::default(),
                &workspace,
                &clean_db,
                &revision,
                1,
            )
            .unwrap()
            .ready;
            assert_eq!(incremental.snapshot_digest, rebuilt.snapshot_digest);
            assert_eq!(incremental.graph_digest, rebuilt.graph_digest);
            assert_ready_facts_equal(&incremental_db, &incremental, &clean_db, &rebuilt);
            let a = CodegraphStore::open(&incremental_db, project)
                .unwrap()
                .current_completeness(workspace.workspace_id)
                .unwrap();
            let b = CodegraphStore::open(&clean_db, project)
                .unwrap()
                .current_completeness(workspace.workspace_id)
                .unwrap();
            assert_eq!(a, b);
        };
        std::fs::write(root.path().join("lib.rs"), "pub fn one() {}\n").unwrap();
        compare();
        std::fs::write(root.path().join("lib.rs"), "pub fn two() {}\n").unwrap();
        compare();
        std::fs::write(root.path().join("notes.md"), "# Notes\n").unwrap();
        compare();
        std::fs::write(
            root.path().join("notes.md"),
            "---\ntitle: Changed\n---\n# Notes\n",
        )
        .unwrap();
        compare();
        std::fs::rename(root.path().join("notes.md"), root.path().join("renamed.md")).unwrap();
        compare();
        std::fs::remove_file(root.path().join("renamed.md")).unwrap();
        compare();
        std::fs::remove_file(root.path().join("lib.rs")).unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub struct Replacement;\n").unwrap();
        compare();
        std::fs::write(root.path().join("report.md"), "# Report\nSee F-001\n").unwrap();
        compare();
        std::fs::write(root.path().join("finding.md"), "# F-001 finding\n").unwrap();
        compare();
        std::fs::remove_file(root.path().join("finding.md")).unwrap();
        compare();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn cargo_manifest_lock_and_rust_target_changes_rebuild_cleanly() {
        let root = tempfile::tempdir().unwrap();
        let incremental_dir = tempfile::tempdir().unwrap();
        let project = uuid::Uuid::new_v4();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"cg_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"cg_fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(project, root.path()).unwrap();
        let incremental_db = project_db_path(incremental_dir.path(), project);
        let revision = AtomicU64::new(1);
        let mut cache = WorkerState::default();
        let mut previous_digest = None;
        let mut compare = || {
            let incremental = index_once(&mut cache, &workspace, &incremental_db, &revision, 1)
                .unwrap()
                .ready;
            let clean_dir = tempfile::tempdir().unwrap();
            let clean_db = project_db_path(clean_dir.path(), project);
            let rebuilt = index_once(
                &mut WorkerState::default(),
                &workspace,
                &clean_db,
                &revision,
                1,
            )
            .unwrap()
            .ready;
            assert_eq!(incremental.snapshot_digest, rebuilt.snapshot_digest);
            assert_eq!(incremental.graph_digest, rebuilt.graph_digest);
            assert_ready_facts_equal(&incremental_db, &incremental, &clean_db, &rebuilt);
            if let Some(previous) = previous_digest.replace(incremental.snapshot_digest) {
                assert_ne!(previous, rebuilt.snapshot_digest);
            }
        };
        compare();
        std::fs::write(root.path().join("Cargo.lock"), "version = 4\n\n[[package]]\nname = \"cg_fixture\"\nversion = \"0.1.0\"\n# changed lock bytes\n").unwrap();
        compare();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        compare();
        std::fs::remove_file(root.path().join("src/main.rs")).unwrap();
        compare();
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname = \"cg_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\ndescription = \"changed\"\n[workspace]\n").unwrap();
        compare();
    }

    fn cargo_fact_nodes(db: &std::path::Path, ready: &rsi_codegraph::ReadySnapshot) -> i64 {
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM cg_nodes WHERE workspace_id = ?1 AND generation = ?2 \
                 AND kind IN ('Crate', 'CargoTarget')",
                rusqlite::params![ready.workspace_id.to_string(), ready.generation],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn rsi_shaped_workspace_with_symlinked_agent_docs_now_indexes() {
        // The shape that failed all 1,590 attempts on the hub: AGENTS.md is the
        // source, CLAUDE.md and GEMINI.md are tracked symlinks to it, and a
        // member crate repeats the pattern.
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("crates/rsid/src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/rsid\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"rsid_fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("crates/rsid/Cargo.toml"),
            "[package]\nname = \"rsid_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("crates/rsid/src/lib.rs"),
            "pub fn alpha() {}\n",
        )
        .unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "# Agents\n").unwrap();
        std::fs::write(root.path().join("crates/rsid/AGENTS.md"), "# Daemon\n").unwrap();
        for link in ["CLAUDE.md", "GEMINI.md", "crates/rsid/CLAUDE.md"] {
            std::os::unix::fs::symlink("AGENTS.md", root.path().join(link)).unwrap();
        }
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);

        let outcome = index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1)
            .expect("symlinked agent docs must not fail the workspace");
        assert!(outcome.published);
        assert_eq!(outcome.skipped_symlinks, 3);
        assert!(outcome.partial_reasons.is_empty());

        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        let paths = store
            .index_manifest(workspace.workspace_id)
            .unwrap()
            .into_iter()
            .map(|source| source.relative_path)
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "AGENTS.md",
                "Cargo.lock",
                "Cargo.toml",
                "crates/rsid/AGENTS.md",
                "crates/rsid/Cargo.toml",
                "crates/rsid/src/lib.rs",
            ]
        );
        let status = store.index_status(workspace.workspace_id).unwrap().unwrap();
        assert_eq!(status.phase, IndexLifecyclePhase::Ready);
        assert_eq!(status.metrics.skipped_symlinks, 3);
        assert!(status.metrics.partial_reasons.is_empty());
        assert!(
            !store
                .search_name(workspace.workspace_id, "alpha", 1)
                .unwrap()
                .nodes
                .is_empty()
        );
        assert!(cargo_fact_nodes(&db, &outcome.ready) > 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn unresolvable_locked_dependency_degrades_to_workspace_packages_and_marks_partial() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        // No Cargo.lock and a crate that no offline registry holds: resolved
        // `cargo metadata --offline --locked` cannot succeed, `--no-deps` can.
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"cg_partial\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n\
             [dependencies]\nabsent-crate-zzz = \"1.2.3\"\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);
        let mut state = WorkerState::default();

        let outcome = index_once(&mut state, &workspace, &db, &revision, 1)
            .expect("an unavailable dependency must not fail the workspace");
        assert!(outcome.published);
        assert_eq!(
            outcome.partial_reasons,
            [IndexPartialReason::CargoMetadataNoDeps]
        );
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        let status = store.index_status(workspace.workspace_id).unwrap().unwrap();
        assert_eq!(status.phase, IndexLifecyclePhase::Ready);
        assert_eq!(
            status.metrics.partial_reasons,
            [IndexPartialReason::CargoMetadataNoDeps]
        );
        assert!(
            !store
                .search_name(workspace.workspace_id, "alpha", 1)
                .unwrap()
                .nodes
                .is_empty()
        );
        assert!(
            cargo_fact_nodes(&db, &outcome.ready) > 0,
            "workspace packages and targets survive the degrade"
        );

        // Unchanged inputs take the confirm path and keep reporting the gap.
        let again = index_once(&mut state, &workspace, &db, &revision, 1).unwrap();
        assert!(!again.published);
        assert_eq!(
            again.partial_reasons,
            [IndexPartialReason::CargoMetadataNoDeps]
        );
        let status = store.index_status(workspace.workspace_id).unwrap().unwrap();
        assert_eq!(
            status.metrics.partial_reasons,
            [IndexPartialReason::CargoMetadataNoDeps]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn unreadable_cargo_manifest_publishes_without_cargo_facts_and_marks_partial() {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        // Valid TOML, invalid manifest (no package name): cargo refuses it in
        // every mode, so there are no Cargo facts to keep.
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nversion = \"0.1.0\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
        let workspace = RegisteredWorkspace::primary(uuid::Uuid::new_v4(), root.path()).unwrap();
        let db = project_db_path(indexes.path(), workspace.project_id);
        let revision = AtomicU64::new(1);

        let outcome = index_once(&mut WorkerState::default(), &workspace, &db, &revision, 1)
            .expect("an unreadable manifest must not fail the workspace");
        assert!(outcome.published);
        assert_eq!(
            outcome.partial_reasons,
            [IndexPartialReason::CargoMetadataUnavailable]
        );
        let store = CodegraphStore::open(&db, workspace.project_id).unwrap();
        let status = store.index_status(workspace.workspace_id).unwrap().unwrap();
        assert_eq!(status.phase, IndexLifecyclePhase::Ready);
        assert_eq!(
            status.metrics.partial_reasons,
            [IndexPartialReason::CargoMetadataUnavailable]
        );
        assert!(
            !store
                .search_name(workspace.workspace_id, "alpha", 1)
                .unwrap()
                .nodes
                .is_empty()
        );
        assert_eq!(cargo_fact_nodes(&db, &outcome.ready), 0);
    }
}
