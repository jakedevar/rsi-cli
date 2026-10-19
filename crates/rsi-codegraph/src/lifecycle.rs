//! Durable index lifecycle in the dedicated, project-bound codegraph database.

use chrono::{SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    CodegraphError, CodegraphStore, ReadySnapshot, Result,
    query::SnapshotSelector,
    staged::{SourceVersion, StagedExtraction},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexLifecyclePhase {
    Queued,
    Building,
    Ready,
    Stale,
    Degraded,
    Failed,
    Recovering,
}

impl IndexLifecyclePhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Building => "Building",
            Self::Ready => "Ready",
            Self::Stale => "Stale",
            Self::Degraded => "Degraded",
            Self::Failed => "Failed",
            Self::Recovering => "Recovering",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "Queued" => Ok(Self::Queued),
            "Building" => Ok(Self::Building),
            "Ready" => Ok(Self::Ready),
            "Stale" => Ok(Self::Stale),
            "Degraded" => Ok(Self::Degraded),
            "Failed" => Ok(Self::Failed),
            "Recovering" => Ok(Self::Recovering),
            _ => Err(CodegraphError::InvalidInput(
                "invalid durable index phase".into(),
            )),
        }
    }
}

/// Why a run published (or attempted) a snapshot that omits part of the
/// workspace's structure. A partial run is still usable; the reason says which
/// facts are absent so a reader never mistakes the gap for "nothing there".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPartialReason {
    /// Resolved `cargo metadata` failed (for example an uncached dependency);
    /// workspace packages and targets came from `--no-deps`, dependency edges
    /// are absent.
    CargoMetadataNoDeps,
    /// `cargo metadata` failed in both modes; the snapshot has no Cargo facts.
    CargoMetadataUnavailable,
}

impl IndexPartialReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CargoMetadataNoDeps => "cargo_metadata_no_deps",
            Self::CargoMetadataUnavailable => "cargo_metadata_unavailable",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "cargo_metadata_no_deps" => Ok(Self::CargoMetadataNoDeps),
            "cargo_metadata_unavailable" => Ok(Self::CargoMetadataUnavailable),
            _ => Err(CodegraphError::InvalidInput(
                "invalid durable partial reason".into(),
            )),
        }
    }

    fn encode(reasons: &[Self]) -> Option<String> {
        (!reasons.is_empty()).then(|| {
            reasons
                .iter()
                .map(|reason| reason.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
    }

    fn decode(value: Option<&str>) -> Result<Vec<Self>> {
        value.filter(|value| !value.is_empty()).map_or_else(
            || Ok(Vec::new()),
            |value| value.split(',').map(Self::parse).collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRunMetrics {
    pub files_discovered: usize,
    pub bytes_discovered: usize,
    pub files_hashed: usize,
    pub files_reused: usize,
    pub files_extracted: usize,
    pub changed_paths: usize,
    pub staged_files: usize,
    pub duration_ms: u64,
    pub overflow_count: u64,
    pub pending_rescan: bool,
    pub rescan_reason: Option<String>,
    /// Included-extension paths skipped because they are symlinks. A skipped
    /// link is never read or followed; it is counted so the gap is visible.
    #[serde(default)]
    pub skipped_symlinks: usize,
    #[serde(default)]
    pub partial_reasons: Vec<IndexPartialReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyFactCounts {
    pub files: usize,
    pub parsed_files: usize,
    pub degraded_files: usize,
    pub nodes: usize,
    pub relations: usize,
    pub unresolved_references: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableIndexStatus {
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub phase: IndexLifecyclePhase,
    pub run_id: Option<Uuid>,
    pub ready: Option<ReadySnapshot>,
    pub ready_counts: Option<ReadyFactCounts>,
    pub source_digest: Option<String>,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub recovery_action: Option<String>,
    pub rescan_reason: Option<String>,
    pub extractor_version: Option<String>,
    pub grammar_version: Option<String>,
    pub rule_version: Option<String>,
    pub normalization_version: Option<String>,
    pub config_digest: Option<String>,
    pub schema_version: i64,
    pub metrics: IndexRunMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredGenerationDetail {
    pub workspace_id: Uuid,
    pub generation: i64,
    pub published_at: String,
    pub detailed_bytes: u64,
    pub current: bool,
}

const MAX_COMPLETED_RUNS_PER_WORKSPACE: i64 = 20;
const MAX_DIAGNOSTIC_BYTES: usize = 1024;

fn bounded_diagnostic(value: &str) -> &str {
    let mut end = value.len().min(MAX_DIAGNOSTIC_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn prune_completed_runs(transaction: &rusqlite::Transaction<'_>, workspace_id: Uuid) -> Result<()> {
    transaction.execute(
        "DELETE FROM cg_index_runs WHERE workspace_id=?1 AND phase!='Building'
         AND run_id NOT IN (
             SELECT run_id FROM cg_index_runs WHERE workspace_id=?1 AND phase!='Building'
             ORDER BY started_at DESC, rowid DESC LIMIT ?2
         )",
        params![workspace_id.to_string(), MAX_COMPLETED_RUNS_PER_WORKSPACE],
    )?;
    Ok(())
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn count(value: usize) -> Result<i64> {
    i64::try_from(value).map_err(|_| CodegraphError::InvalidInput("index count overflow".into()))
}

fn millis(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| CodegraphError::InvalidInput("index duration overflow".into()))
}

fn usize_count(value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| CodegraphError::InvalidInput("invalid index count".into()))
}

impl CodegraphStore {
    /// Checkpoint and truncate WAL before a physical write-budget decision.
    /// Returns false when an active reader pins the WAL snapshot.
    pub fn checkpoint_write_wal(&self) -> Result<bool> {
        let (busy, _, _): (i64, i64, i64) =
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        Ok(busy == 0)
    }

    /// Limit main-file growth for this writer connection. The caller budgets
    /// WAL and other project files separately because `max_page_count` covers
    /// only this database's main file.
    pub fn constrain_database_bytes(&self, max_bytes: u64) -> Result<()> {
        let page_size: u64 = self
            .connection
            .pragma_query_value(None, "page_size", |row| row.get(0))?;
        let pages = (max_bytes / page_size).min(4_294_967_294);
        if pages == 0 {
            return Err(CodegraphError::InvalidInput(
                "codegraph write budget has no database page".into(),
            ));
        }
        self.connection
            .pragma_update(None, "max_page_count", pages)?;
        let applied: u64 = self
            .connection
            .pragma_query_value(None, "max_page_count", |row| row.get(0))?;
        if applied > pages {
            return Err(CodegraphError::InvalidInput(
                "codegraph database already exceeds write budget".into(),
            ));
        }
        self.connection
            .pragma_update(None, "journal_size_limit", 0)?;
        Ok(())
    }

    /// Mark interrupted runs for full reconciliation while retaining ready heads.
    /// Returns the workspace IDs that need a cold rescan.
    pub fn recover_interrupted_index_runs(&mut self) -> Result<Vec<Uuid>> {
        let transaction = self.connection.transaction()?;
        let ids = {
            let mut statement = transaction.prepare(
                "SELECT workspace_id FROM cg_index_state WHERE phase='Building' ORDER BY workspace_id",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let at = now();
        transaction.execute(
            "UPDATE cg_index_runs SET phase='Interrupted',finished_at=?1,error='interrupted by restart'
             WHERE phase='Building'",
            [&at],
        )?;
        transaction.execute(
            "UPDATE cg_index_state SET phase='Recovering',run_id=NULL,pending_rescan=1,
             rescan_reason='interrupted_run',
             recovery_action='cold full inventory after interrupted run',updated_at=?1
             WHERE phase='Building'",
            [&at],
        )?;
        transaction.commit()?;
        ids.into_iter()
            .map(|id| {
                Uuid::parse_str(&id)
                    .map_err(|error| CodegraphError::InvalidInput(error.to_string()))
            })
            .collect()
    }

    /// Begin a durable run before extraction or staging work starts.
    pub fn begin_index_run(
        &mut self,
        workspace_id: Uuid,
        source_digest: &str,
        extraction: &StagedExtraction,
    ) -> Result<Uuid> {
        if source_digest.len() != 64 || !source_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CodegraphError::InvalidInput(
                "invalid index source digest".into(),
            ));
        }
        let run_id = Uuid::new_v4();
        let at = now();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE cg_index_runs SET phase='Interrupted',finished_at=?2,
             error='superseded by a new full inventory'
             WHERE workspace_id=?1 AND phase='Building'",
            params![workspace_id.to_string(), at],
        )?;
        prune_completed_runs(&transaction, workspace_id)?;
        transaction.execute(
            "INSERT INTO cg_index_runs(run_id,workspace_id,phase,started_at,source_digest)
             VALUES (?1,?2,'Building',?3,?4)",
            params![
                run_id.to_string(),
                workspace_id.to_string(),
                at,
                source_digest
            ],
        )?;
        transaction.execute(
            "INSERT INTO cg_index_state(workspace_id,phase,run_id,source_digest,last_attempt_at,updated_at,
                extractor_version,grammar_version,rule_version,normalization_version,config_digest)
             VALUES (?1,'Building',?2,?3,?4,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(workspace_id) DO UPDATE SET phase='Building',run_id=excluded.run_id,
                source_digest=excluded.source_digest,last_attempt_at=excluded.last_attempt_at,
                updated_at=excluded.updated_at,last_error=NULL,pending_rescan=0,rescan_reason=NULL,
                files_hashed=0,files_reused=0,files_extracted=0,
                extractor_version=excluded.extractor_version,grammar_version=excluded.grammar_version,
                rule_version=excluded.rule_version,normalization_version=excluded.normalization_version,
                config_digest=excluded.config_digest",
            params![workspace_id.to_string(), run_id.to_string(), source_digest, at,
                extraction.extraction.extractor.version, extraction.grammar_version,
                extraction.rule_version, extraction.normalization_version, extraction.config_digest],
        )?;
        transaction.commit()?;
        Ok(run_id)
    }

    /// Finish a run. `manifest` is the inventory a successful publish made the
    /// head; it is not stored, because the head's structure blocks already carry
    /// every file's path and source digest (#2351, see `index_manifest`).
    pub fn finish_index_run(
        &mut self,
        run_id: Uuid,
        workspace_id: Uuid,
        phase: IndexLifecyclePhase,
        metrics: &IndexRunMetrics,
        manifest: Option<&[SourceVersion]>,
        error: Option<&str>,
    ) -> Result<()> {
        if !matches!(
            phase,
            IndexLifecyclePhase::Ready
                | IndexLifecyclePhase::Stale
                | IndexLifecyclePhase::Degraded
                | IndexLifecyclePhase::Failed
        ) {
            return Err(CodegraphError::InvalidInput(
                "invalid terminal index phase".into(),
            ));
        }
        if manifest.is_some()
            && !matches!(
                phase,
                IndexLifecyclePhase::Ready | IndexLifecyclePhase::Stale
            )
        {
            return Err(CodegraphError::InvalidInput(
                "manifest requires a published ready run".into(),
            ));
        }
        let at = now();
        let error = error.map(bounded_diagnostic);
        let transaction = self.connection.transaction()?;
        let updated = transaction.execute(
            "UPDATE cg_index_runs SET phase=?3,finished_at=?4,error=?5
             WHERE run_id=?1 AND workspace_id=?2 AND phase='Building'",
            params![
                run_id.to_string(),
                workspace_id.to_string(),
                if matches!(
                    phase,
                    IndexLifecyclePhase::Ready | IndexLifecyclePhase::Stale
                ) {
                    "Ready"
                } else {
                    "Failed"
                },
                at,
                error
            ],
        )?;
        if updated != 1 {
            return Err(CodegraphError::InvalidInput(
                "index run is not active".into(),
            ));
        }
        let state_updated = transaction.execute(
            "UPDATE cg_index_state SET phase=?3,run_id=NULL,
                last_success_at=CASE WHEN ?4=1 THEN ?5 ELSE last_success_at END,
                last_duration_ms=?6,files_discovered=?7,bytes_discovered=?8,
                changed_paths=?9,staged_files=?10,overflow_count=?11,pending_rescan=?12,
                last_error=?13,files_hashed=?14,files_reused=?15,files_extracted=?16,
                rescan_reason=?17,skipped_symlinks=?18,partial_reasons=?19,updated_at=?5
             WHERE workspace_id=?1 AND run_id=?2",
            params![
                workspace_id.to_string(),
                run_id.to_string(),
                phase.as_str(),
                i64::from(manifest.is_some()),
                at,
                millis(metrics.duration_ms)?,
                count(metrics.files_discovered)?,
                count(metrics.bytes_discovered)?,
                count(metrics.changed_paths)?,
                count(metrics.staged_files)?,
                millis(metrics.overflow_count)?,
                i64::from(metrics.pending_rescan),
                error,
                count(metrics.files_hashed)?,
                count(metrics.files_reused)?,
                count(metrics.files_extracted)?,
                metrics.rescan_reason.as_deref(),
                count(metrics.skipped_symlinks)?,
                IndexPartialReason::encode(&metrics.partial_reasons)
            ],
        )?;
        if state_updated != 1 {
            return Err(CodegraphError::InvalidInput(
                "index state does not own run".into(),
            ));
        }
        prune_completed_runs(&transaction, workspace_id)?;
        transaction.commit()?;
        Ok(())
    }

    /// Read a durable status with the current ready head, if one exists.
    /// Counts facts in one retained ready generation, including historical
    /// snapshots. Legacy strict-bundle generations have no completeness row.
    pub fn ready_counts_at(&self, workspace_id: Uuid, generation: i64) -> Result<ReadyFactCounts> {
        self.query(workspace_id, SnapshotSelector::Generation(generation))?;
        let workspace = workspace_id.to_string();
        let completeness: Option<(i64, i64, i64)> = self
            .connection
            .query_row(
                "SELECT total_files,parsed_files,degraded_files FROM cg_snapshot_completeness WHERE workspace_id=?1 AND generation=?2",
                params![workspace, generation],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let count_rows = |sql: &str| -> Result<usize> {
            let rows: i64 =
                self.connection
                    .query_row(sql, params![workspace, generation], |row| row.get(0))?;
            usize_count(rows)
        };
        let files = count_rows(
            "SELECT count(*) FROM cg_manifest m JOIN cg_blocks b ON b.block_id=m.block_id
             WHERE m.workspace_id=?1 AND m.generation=?2 AND b.kind='S'",
        )?;
        let (parsed_files, degraded_files) = match completeness {
            Some((total, parsed, degraded)) => {
                if usize_count(total)? != files {
                    return Err(CodegraphError::InvalidInput(
                        "snapshot completeness count mismatch".into(),
                    ));
                }
                (usize_count(parsed)?, usize_count(degraded)?)
            }
            None => (files, 0),
        };
        Ok(ReadyFactCounts {
            files,
            parsed_files,
            degraded_files,
            nodes: count_rows(
                "SELECT count(*) FROM cg_manifest m JOIN cg_block_nodes n ON n.block_id=m.block_id
                 WHERE m.workspace_id=?1 AND m.generation=?2",
            )?,
            relations: count_rows(
                "SELECT count(*) FROM cg_manifest m JOIN cg_block_relations r ON r.block_id=m.block_id
                 WHERE m.workspace_id=?1 AND m.generation=?2",
            )?,
            unresolved_references: count_rows(
                "SELECT count(*) FROM cg_manifest m JOIN cg_block_unresolved u ON u.block_id=m.block_id
                 WHERE m.workspace_id=?1 AND m.generation=?2",
            )?,
        })
    }

    pub fn index_status(&self, workspace_id: Uuid) -> Result<Option<DurableIndexStatus>> {
        let row = self
            .connection
            .query_row(
                "SELECT phase,run_id,source_digest,last_attempt_at,last_success_at,last_error,
                    recovery_action,files_discovered,bytes_discovered,changed_paths,
                    staged_files,last_duration_ms,overflow_count,pending_rescan,
                    extractor_version,grammar_version,rule_version,normalization_version,config_digest,
                    files_hashed,files_reused,files_extracted,rescan_reason,
                    skipped_symlinks,partial_reasons
             FROM cg_index_state WHERE workspace_id=?1",
                [workspace_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                        row.get::<_, i64>(12)?,
                        row.get::<_, i64>(13)?,
                        row.get::<_, Option<String>>(14)?,
                        row.get::<_, Option<String>>(15)?,
                        row.get::<_, Option<String>>(16)?,
                        row.get::<_, Option<String>>(17)?,
                        row.get::<_, Option<String>>(18)?,
                        row.get::<_, i64>(19)?,
                        row.get::<_, i64>(20)?,
                        row.get::<_, i64>(21)?,
                        row.get::<_, Option<String>>(22)?,
                        row.get::<_, i64>(23)?,
                        row.get::<_, Option<String>>(24)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            phase,
            run_id,
            source_digest,
            last_attempt_at,
            last_success_at,
            last_error,
            recovery_action,
            files,
            bytes,
            changed,
            staged,
            duration,
            overflow,
            pending,
            extractor_version,
            grammar_version,
            rule_version,
            normalization_version,
            config_digest,
            hashed,
            reused,
            extracted,
            rescan_reason,
            skipped_symlinks,
            partial_reasons,
        )) = row
        else {
            return Ok(None);
        };
        let ready = match self.current_ready(workspace_id) {
            Ok(ready) => Some(ready),
            Err(CodegraphError::NoReadySnapshot) => None,
            Err(error) => return Err(error),
        };
        let ready_counts = ready
            .as_ref()
            .map(|ready| self.ready_counts_at(workspace_id, ready.generation))
            .transpose()?;
        Ok(Some(DurableIndexStatus {
            project_id: self.project_id,
            workspace_id,
            phase: IndexLifecyclePhase::parse(&phase)?,
            run_id: run_id
                .map(|id| {
                    Uuid::parse_str(&id)
                        .map_err(|error| CodegraphError::InvalidInput(error.to_string()))
                })
                .transpose()?,
            ready,
            ready_counts,
            source_digest,
            last_attempt_at,
            last_success_at,
            last_error,
            recovery_action,
            rescan_reason: rescan_reason.clone(),
            extractor_version,
            grammar_version,
            rule_version,
            normalization_version,
            config_digest,
            schema_version: super::SCHEMA_VERSION,
            metrics: IndexRunMetrics {
                files_discovered: usize_count(files)?,
                bytes_discovered: usize_count(bytes)?,
                files_hashed: usize_count(hashed)?,
                files_reused: usize_count(reused)?,
                files_extracted: usize_count(extracted)?,
                changed_paths: usize_count(changed)?,
                staged_files: usize_count(staged)?,
                duration_ms: u64::try_from(duration.unwrap_or(0))
                    .map_err(|_| CodegraphError::InvalidInput("invalid index duration".into()))?,
                overflow_count: u64::try_from(overflow)
                    .map_err(|_| CodegraphError::InvalidInput("invalid overflow count".into()))?,
                pending_rescan: pending != 0,
                rescan_reason,
                skipped_symlinks: usize_count(skipped_symlinks)?,
                partial_reasons: IndexPartialReason::decode(partial_reasons.as_deref())?,
            },
        }))
    }

    /// Exact source bytes represented by the last successfully published head,
    /// read from the head's structure blocks (#2351): a workspace stores no
    /// manifest of its own.
    pub fn index_manifest(&self, workspace_id: Uuid) -> Result<Vec<SourceVersion>> {
        let mut statement = self.connection.prepare(
            "SELECT b.path,b.source_digest
             FROM cg_workspace_heads h
               JOIN cg_manifest m ON m.workspace_id=h.workspace_id AND m.generation=h.generation
               JOIN cg_blocks b ON b.block_id=m.block_id
             WHERE h.workspace_id=?1 AND b.kind='S' ORDER BY b.path",
        )?;
        let rows = statement.query_map([workspace_id.to_string()], |row| {
            Ok(SourceVersion {
                relative_path: row.get(0)?,
                source_digest: row.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Account for the published generation and preserve identities across
    /// later detailed-history pruning.
    pub fn record_published_generation(
        &mut self,
        workspace_id: Uuid,
        generation: i64,
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let current: Option<i64> = transaction
            .query_row(
                "SELECT generation FROM cg_workspace_heads WHERE workspace_id=?1",
                [workspace_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if current != Some(generation) {
            return Err(CodegraphError::InvalidInput(
                "generation is not current ready head".into(),
            ));
        }
        let bytes = measure_generation(&transaction, workspace_id, generation)?;
        transaction.execute(
            "INSERT INTO cg_generation_detail(workspace_id,generation,published_at,detailed_bytes)
             VALUES (?1,?2,?3,?4) ON CONFLICT(workspace_id,generation) DO UPDATE SET
             published_at=COALESCE(cg_generation_detail.published_at,excluded.published_at),
             detailed_bytes=excluded.detailed_bytes",
            params![workspace_id.to_string(), generation, now(), bytes],
        )?;
        release_returning_identities(&transaction, workspace_id, generation)?;
        transaction.commit()?;
        Ok(())
    }

    /// Refresh legacy or crash-interrupted accounting before selecting history
    /// for retention. The S1 ready-head commit can precede this v5 write.
    pub fn generation_details(&mut self) -> Result<Vec<StoredGenerationDetail>> {
        let transaction = self.connection.transaction()?;
        let unaccounted_heads = {
            let mut statement = transaction.prepare(
                "SELECT h.workspace_id,h.generation FROM cg_workspace_heads h
                 LEFT JOIN cg_generation_detail d ON d.workspace_id=h.workspace_id
                   AND d.generation=h.generation WHERE d.workspace_id IS NULL",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        transaction.execute(
            "INSERT OR IGNORE INTO cg_generation_detail(workspace_id,generation,published_at,detailed_bytes)
             SELECT workspace_id,generation,NULL,0 FROM cg_snapshots WHERE ready=1",
            [],
        )?;
        for (id, generation) in unaccounted_heads {
            let workspace_id = Uuid::parse_str(&id)
                .map_err(|error| CodegraphError::InvalidInput(error.to_string()))?;
            release_returning_identities(&transaction, workspace_id, generation)?;
        }
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT d.workspace_id,d.generation,d.published_at,d.detailed_bytes,
                    h.generation=d.generation
                 FROM cg_generation_detail d LEFT JOIN cg_workspace_heads h
                   ON h.workspace_id=d.workspace_id ORDER BY d.workspace_id,d.generation",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<bool>>(4)?.unwrap_or(false),
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut details = Vec::with_capacity(rows.len());
        for (id, generation, published_at, stored_bytes, current) in rows {
            let workspace_id = Uuid::parse_str(&id)
                .map_err(|error| CodegraphError::InvalidInput(error.to_string()))?;
            let bytes = if stored_bytes == 0 {
                let measured = measure_generation(&transaction, workspace_id, generation)?;
                transaction.execute(
                    "UPDATE cg_generation_detail SET detailed_bytes=?3 WHERE workspace_id=?1 AND generation=?2",
                    params![id,generation,measured],
                )?;
                measured
            } else {
                stored_bytes
            };
            let missing_at = published_at.is_none();
            let at = published_at.unwrap_or_else(now);
            if missing_at {
                transaction.execute(
                    "UPDATE cg_generation_detail SET published_at=?3 WHERE workspace_id=?1 AND generation=?2 AND published_at IS NULL",
                    params![id,generation,at],
                )?;
            }
            details.push(StoredGenerationDetail {
                workspace_id,
                generation,
                published_at: at,
                detailed_bytes: u64::try_from(bytes).map_err(|_| {
                    CodegraphError::InvalidInput("invalid detailed byte count".into())
                })?,
                current,
            });
        }
        transaction.commit()?;
        Ok(details)
    }

    /// Remove one prior ready generation and keep durable linked identities.
    pub fn prune_detailed_generation(&mut self, workspace_id: Uuid, generation: i64) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let current: Option<i64> = transaction
            .query_row(
                "SELECT generation FROM cg_workspace_heads WHERE workspace_id=?1",
                [workspace_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if current == Some(generation) {
            return Err(CodegraphError::InvalidInput(
                "cannot prune current ready head".into(),
            ));
        }
        let ready: Option<i64> = transaction
            .query_row(
                "SELECT ready FROM cg_snapshots WHERE workspace_id=?1 AND generation=?2",
                params![workspace_id.to_string(), generation],
                |row| row.get(0),
            )
            .optional()?;
        if ready != Some(1) {
            return Err(CodegraphError::InvalidInput(
                "generation is not retained ready history".into(),
            ));
        }
        tombstone_leaving_identities(&transaction, workspace_id, generation)?;
        let id = workspace_id.to_string();
        // The generation's manifest goes first; only blocks that no other
        // generation or workspace shares are deleted with it.
        crate::blocks::detach_generation(&transaction, &id, generation)?;
        for table in [
            "cg_snapshot_completeness",
            "cg_generation_detail",
            "cg_snapshots",
        ] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE workspace_id=?1 AND generation=?2"),
                params![id, generation],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Count pages available for reclamation after detailed-history pruning.
    pub fn reclaimable_bytes(&self) -> Result<u64> {
        let free_pages: u64 =
            self.connection
                .pragma_query_value(None, "freelist_count", |row| row.get(0))?;
        let page_size: u64 = self
            .connection
            .pragma_query_value(None, "page_size", |row| row.get(0))?;
        Ok(free_pages.saturating_mul(page_size))
    }

    /// Reclaim pages after detailed-history pruning when existing readers allow
    /// a WAL checkpoint. A busy reader keeps using its prior snapshot; the
    /// caller can retry maintenance after a later publication.
    pub fn compact_pruned_history(&mut self, min_reclaimable_bytes: u64) -> Result<bool> {
        let (busy, _, _): (i64, i64, i64) =
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        if busy != 0 {
            return Ok(false);
        }
        if self.reclaimable_bytes()? < min_reclaimable_bytes {
            return Ok(true);
        }
        self.connection.execute_batch("VACUUM")?;
        let (busy, _, _): (i64, i64, i64) =
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        Ok(busy == 0)
    }

    /// Surface post-publication maintenance failure without losing the head.
    pub fn mark_index_degraded(&mut self, workspace_id: Uuid, error: &str) -> Result<()> {
        let updated = self.connection.execute(
            "UPDATE cg_index_state SET phase='Degraded',last_error=?2,
             rescan_reason='maintenance_failure',updated_at=?3
             WHERE workspace_id=?1 AND run_id IS NULL AND phase IN ('Ready','Stale')",
            params![workspace_id.to_string(), bounded_diagnostic(error), now()],
        )?;
        if updated != 1 {
            return Err(CodegraphError::InvalidInput(
                "ready index state unavailable".into(),
            ));
        }
        Ok(())
    }

    /// Persist a failed discovery or metadata preflight without claiming that
    /// extraction began. The previous ready head and source manifest remain.
    pub fn record_index_preflight_failure(
        &mut self,
        workspace_id: Uuid,
        had_ready: bool,
        error: &str,
        overflow_count: u64,
    ) -> Result<()> {
        let at = now();
        self.connection.execute(
            "INSERT INTO cg_index_state(workspace_id,phase,last_attempt_at,last_error,pending_rescan,overflow_count,rescan_reason,updated_at)
             VALUES (?1,?2,?3,?4,1,?5,'preflight_failure',?3)
             ON CONFLICT(workspace_id) DO UPDATE SET phase=excluded.phase,run_id=NULL,
                 last_attempt_at=excluded.last_attempt_at,last_error=excluded.last_error,
                 pending_rescan=1,overflow_count=excluded.overflow_count,
                 rescan_reason=excluded.rescan_reason,updated_at=excluded.updated_at",
            params![workspace_id.to_string(),
                if had_ready { "Degraded" } else { "Failed" }, at, bounded_diagnostic(error),
                i64::try_from(overflow_count).map_err(|_| CodegraphError::InvalidInput("overflow count overflow".into()))?],
        )?;
        Ok(())
    }

    /// Confirm unchanged, exact-byte input after a previous failure without
    /// claiming a new publication or replacing its source manifest.
    pub fn confirm_index_ready(
        &mut self,
        workspace_id: Uuid,
        source_digest: &str,
        overflow_count: u64,
    ) -> Result<bool> {
        let previous: Option<(String, Option<String>, i64)> = self
            .connection
            .query_row(
                "SELECT phase,last_error,pending_rescan FROM cg_index_state WHERE workspace_id=?1",
                [workspace_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((phase, error, pending)) = previous else {
            return Err(CodegraphError::InvalidInput(
                "index state unavailable".into(),
            ));
        };
        let changed = phase != "Ready" || error.is_some() || pending != 0;
        let updated = self.connection.execute(
            "UPDATE cg_index_state SET phase='Ready',source_digest=?2,last_attempt_at=?3,
                last_error=NULL,pending_rescan=0,rescan_reason=NULL,
                overflow_count=?4,updated_at=?3
             WHERE workspace_id=?1 AND run_id IS NULL AND EXISTS
                (SELECT 1 FROM cg_workspace_heads WHERE workspace_id=?1)",
            params![
                workspace_id.to_string(),
                source_digest,
                now(),
                i64::try_from(overflow_count)
                    .map_err(|_| CodegraphError::InvalidInput("overflow count overflow".into()))?
            ],
        )?;
        if updated != 1 {
            return Err(CodegraphError::InvalidInput(
                "ready head unavailable".into(),
            ));
        }
        Ok(changed)
    }
}

/// Bytes this generation adds to the database. Blocks shared with another
/// generation or workspace are counted where they were first stored, so the sum
/// over retained generations tracks the physical size instead of repeating one
/// graph per generation.
fn measure_generation(
    transaction: &rusqlite::Transaction<'_>,
    workspace_id: Uuid,
    generation: i64,
) -> Result<i64> {
    crate::blocks::marginal_generation_bytes(transaction, &workspace_id.to_string(), generation)
}

/// SQL predicate (#2351, replacing the head-only probe of #2464): the node
/// `n` is still held by a retained generation of workspace `?1` other than the
/// pruned `?2`. The probe starts from the node's own blocks
/// (`cg_block_nodes_id`) and checks each against the primary key of the
/// workspace's other retained manifests, so one prune stays linear in the
/// generation's identities (generations x blocks per node, not nodes x blocks
/// of a manifest: the old head walk ran about 20 minutes on the hub's 12k-block,
/// 218k-node rsi index).
const OTHER_GENERATION_HOLDS_NODE_SQL: &str = "EXISTS (
            SELECT 1 FROM cg_block_nodes other
             WHERE other.node_id=n.node_id
               AND EXISTS (SELECT 1 FROM cg_snapshots s
                             JOIN cg_manifest om
                               ON om.workspace_id=s.workspace_id
                              AND om.generation=s.generation
                              AND om.block_id=other.block_id
                            WHERE s.workspace_id=?1 AND s.ready=1 AND s.generation<>?2)
          )";

/// [`OTHER_GENERATION_HOLDS_NODE_SQL`] for relations (`cg_block_relations_id`).
const OTHER_GENERATION_HOLDS_RELATION_SQL: &str = "EXISTS (
            SELECT 1 FROM cg_block_relations other
             WHERE other.relation_id=r.relation_id
               AND EXISTS (SELECT 1 FROM cg_snapshots s
                             JOIN cg_manifest om
                               ON om.workspace_id=s.workspace_id
                              AND om.generation=s.generation
                              AND om.block_id=other.block_id
                            WHERE s.workspace_id=?1 AND s.ready=1 AND s.generation<>?2)
          )";

/// A generation is about to be pruned. The identity ledger (#2351) holds only
/// identities that left every retained generation, so the ones this generation
/// alone still holds are recorded as tombstones; identities another retained
/// generation holds are derived from its blocks and need no row.
fn tombstone_leaving_identities(
    transaction: &rusqlite::Transaction<'_>,
    workspace_id: Uuid,
    generation: i64,
) -> Result<()> {
    let id = workspace_id.to_string();
    transaction.execute(
        &format!("INSERT INTO cg_retained_nodes(workspace_id,node_id,node_key,kind,name,last_detailed_generation,tombstoned)
          SELECT ?1,n.node_id,n.node_key,n.kind,n.name,?2,1
          FROM cg_manifest m JOIN cg_block_nodes n ON n.block_id=m.block_id
          WHERE m.workspace_id=?1 AND m.generation=?2 AND NOT {OTHER_GENERATION_HOLDS_NODE_SQL}
          ON CONFLICT(workspace_id,node_id) DO UPDATE SET
            last_detailed_generation=MAX(cg_retained_nodes.last_detailed_generation,
              excluded.last_detailed_generation)"),
        params![id,generation],
    )?;
    transaction.execute(
        &format!("INSERT INTO cg_retained_relations(workspace_id,relation_id,relation_key,kind,source_id,target_id,last_detailed_generation,tombstoned)
          SELECT ?1,r.relation_id,r.relation_key,r.kind,r.source_id,r.target_id,?2,1
          FROM cg_manifest m JOIN cg_block_relations r ON r.block_id=m.block_id
          WHERE m.workspace_id=?1 AND m.generation=?2 AND NOT {OTHER_GENERATION_HOLDS_RELATION_SQL}
          ON CONFLICT(workspace_id,relation_id) DO UPDATE SET
            last_detailed_generation=MAX(cg_retained_relations.last_detailed_generation,
              excluded.last_detailed_generation)"),
        params![id,generation],
    )?;
    Ok(())
}

/// A generation was published. Identities live in the head's blocks and have
/// no ledger row (#2351); the only rows that can be wrong are tombstones of
/// identities this generation brings back. Those can only be in blocks the
/// previous retained generation did not hold, so a publish of unchanged content
/// reads no identity at all, and a workspace with no tombstones skips the work.
fn release_returning_identities(
    transaction: &rusqlite::Transaction<'_>,
    workspace_id: Uuid,
    generation: i64,
) -> Result<()> {
    let id = workspace_id.to_string();
    let previous: Option<i64> = transaction.query_row(
        "SELECT MAX(generation) FROM cg_snapshots WHERE workspace_id=?1 AND ready=1 AND generation<?2",
        params![id, generation],
        |row| row.get(0),
    )?;
    let previous = previous.unwrap_or(-1);
    for (ledger, key, source, source_key) in [
        ("cg_retained_nodes", "node_id", "cg_block_nodes", "node_id"),
        (
            "cg_retained_relations",
            "relation_id",
            "cg_block_relations",
            "relation_id",
        ),
    ] {
        let tombstones: bool = transaction.query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM {ledger} WHERE workspace_id=?1)"),
            [&id],
            |row| row.get(0),
        )?;
        if !tombstones {
            continue;
        }
        transaction.execute(
            &format!(
                "DELETE FROM {ledger} WHERE workspace_id=?1 AND {key} IN (
                   SELECT x.{source_key} FROM cg_manifest m JOIN {source} x ON x.block_id=m.block_id
                   WHERE m.workspace_id=?1 AND m.generation=?2
                     AND NOT EXISTS (SELECT 1 FROM cg_manifest p
                                      WHERE p.workspace_id=?1 AND p.generation=?3
                                        AND p.block_id=m.block_id))"
            ),
            params![id, generation, previous],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExtractionContract, ExtractionMode, ExtractorIdentity};

    fn extraction() -> StagedExtraction {
        StagedExtraction {
            extraction: ExtractionContract {
                mode: ExtractionMode::ExtractedV1_0,
                extractor: ExtractorIdentity {
                    name: "test".into(),
                    version: "1".into(),
                },
            },
            grammar_version: "1".into(),
            rule_version: "1".into(),
            normalization_version: "1".into(),
            config_digest: "a".repeat(64),
        }
    }

    #[test]
    fn interrupted_run_reopens_as_recovering_without_a_ready_head() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codegraph.sqlite");
        let project_id = Uuid::new_v4();
        let workspace_id = CodegraphStore::workspace_id(project_id, "primary").unwrap();
        let run_id = {
            let mut store = CodegraphStore::open(&path, project_id).unwrap();
            let run_id = store
                .begin_index_run(workspace_id, &"a".repeat(64), &extraction())
                .unwrap();
            let status = store.index_status(workspace_id).unwrap().unwrap();
            assert_eq!(status.phase, IndexLifecyclePhase::Building);
            assert_eq!(status.run_id, Some(run_id));
            assert_eq!(status.project_id, project_id);
            assert_eq!(status.extractor_version.as_deref(), Some("1"));
            assert_eq!(status.schema_version, 10);
            run_id
        };
        let mut reopened = CodegraphStore::open(&path, project_id).unwrap();
        assert_eq!(
            reopened.recover_interrupted_index_runs().unwrap(),
            vec![workspace_id]
        );
        let recovered = reopened.index_status(workspace_id).unwrap().unwrap();
        assert_eq!(recovered.phase, IndexLifecyclePhase::Recovering);
        assert!(recovered.ready.is_none());
        assert!(recovered.metrics.pending_rescan);
        let old_phase: String = reopened
            .connection
            .query_row(
                "SELECT phase FROM cg_index_runs WHERE run_id=?1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_phase, "Interrupted");
    }

    #[test]
    fn completed_run_history_and_diagnostics_are_bounded_per_workspace() {
        let root = tempfile::tempdir().unwrap();
        let project_id = Uuid::new_v4();
        let mut store =
            CodegraphStore::open(root.path().join("codegraph.sqlite"), project_id).unwrap();
        let first = CodegraphStore::workspace_id(project_id, "first").unwrap();
        let second = CodegraphStore::workspace_id(project_id, "second").unwrap();
        let error = "é".repeat(MAX_DIAGNOSTIC_BYTES);
        let metrics = IndexRunMetrics {
            files_discovered: 0,
            bytes_discovered: 0,
            files_hashed: 0,
            files_reused: 0,
            files_extracted: 0,
            changed_paths: 0,
            staged_files: 0,
            duration_ms: 0,
            overflow_count: 0,
            pending_rescan: true,
            rescan_reason: Some("run_failed".into()),
            skipped_symlinks: 0,
            partial_reasons: Vec::new(),
        };
        for workspace_id in [first, second] {
            for _ in 0..25 {
                let run = store
                    .begin_index_run(workspace_id, &"a".repeat(64), &extraction())
                    .unwrap();
                store
                    .finish_index_run(
                        run,
                        workspace_id,
                        IndexLifecyclePhase::Failed,
                        &metrics,
                        None,
                        Some(&error),
                    )
                    .unwrap();
            }
            let (count, max_error_bytes): (i64, i64) = store
                .connection
                .query_row(
                    "SELECT count(*),max(length(cast(error AS blob))) FROM cg_index_runs WHERE workspace_id=?1",
                    [workspace_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(count, MAX_COMPLETED_RUNS_PER_WORKSPACE);
            assert!(max_error_bytes <= MAX_DIAGNOSTIC_BYTES as i64);
            let status = store.index_status(workspace_id).unwrap().unwrap();
            assert_eq!(status.last_error.unwrap().len(), MAX_DIAGNOSTIC_BYTES);
        }
    }

    /// #2464: seed `blocks` one-node, one-relation blocks; generation 1 holds
    /// the first `blocks`, the head (generation 2) the last `blocks`, so half of
    /// generation 1 is still current and half is gone. Returns the SQLite VM
    /// instructions retaining generation 1's identities took, and how many of
    /// its nodes and relations were tombstoned.
    fn prune_identity_cost(blocks: i64) -> (u64, i64, i64) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let root = tempfile::tempdir().unwrap();
        let mut store =
            CodegraphStore::open(root.path().join("codegraph.sqlite"), Uuid::new_v4()).unwrap();
        let workspace = Uuid::new_v4();
        let id = workspace.to_string();
        store
            .connection
            .execute_batch(&format!(
                "WITH RECURSIVE k(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM k WHERE i < {total})
                 INSERT INTO cg_blocks(block_id,kind,digest,path,source_digest,bytes,refs)
                   SELECT i,'S','d'||i,'p'||i,'s'||i,1,1 FROM k;
                 INSERT INTO cg_block_nodes(block_id,node_id,node_key,kind,name,start_byte,end_byte,
                     start_line,start_column,end_line,end_column,provenance)
                   SELECT block_id,'n'||block_id,'k'||block_id,'fn','f'||block_id,0,1,0,0,0,1,'Extracted'
                   FROM cg_blocks;
                 INSERT INTO cg_block_relations(block_id,relation_id,relation_key,kind,source_id,target_id,provenance)
                   SELECT block_id,'r'||block_id,'rk'||block_id,'calls','n'||block_id,'n1','Extracted'
                   FROM cg_blocks;
                 INSERT INTO cg_snapshots(project_id,repository_id,workspace_id,generation,
                     snapshot_digest,graph_digest,ready)
                   VALUES('p','r','{id}',1,'a','b',1),('p','r','{id}',2,'a','b',1);
                 INSERT INTO cg_workspace_heads(workspace_id,generation) VALUES('{id}',2);
                 INSERT INTO cg_manifest SELECT '{id}',1,block_id FROM cg_blocks WHERE block_id<={blocks};
                 INSERT INTO cg_manifest SELECT '{id}',2,block_id FROM cg_blocks WHERE block_id>{half};",
                total = blocks + blocks / 2,
                half = blocks / 2,
            ))
            .unwrap();
        let counter = Arc::new(AtomicU64::new(0));
        let handle = Arc::clone(&counter);
        store.connection.progress_handler(
            1,
            Some(move || {
                handle.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let transaction = store.connection.transaction().unwrap();
        tombstone_leaving_identities(&transaction, workspace, 1).unwrap();
        transaction.commit().unwrap();
        store.connection.progress_handler(1, None::<fn() -> bool>);
        let tombstoned = |table: &str| -> i64 {
            store
                .connection
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE tombstoned=1"),
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        (
            counter.load(Ordering::Relaxed),
            tombstoned("cg_retained_nodes"),
            tombstoned("cg_retained_relations"),
        )
    }

    /// #2464: pruning a generation is linear in its identities, not
    /// identities x head blocks. Quadrupling the blocks quadruples the work
    /// (the old head-manifest walk grew 15x here), and the tombstones stay
    /// exact: only identities the head no longer holds are tombstoned.
    #[test]
    fn prune_identity_retention_is_linear_in_the_generation_2464() {
        let (small, small_nodes, small_relations) = prune_identity_cost(100);
        let (large, large_nodes, large_relations) = prune_identity_cost(400);
        assert_eq!((small_nodes, small_relations), (50, 50));
        assert_eq!((large_nodes, large_relations), (200, 200));
        assert!(
            large < small * 6,
            "prune cost grew {large} / {small} for 4x the blocks: not linear"
        );
        assert!(
            large < 400 * 2 * 250,
            "{large} VM instructions for 800 identities: a per-identity walk of the head"
        );
    }

    #[test]
    fn compaction_reclaims_deleted_database_pages() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codegraph.sqlite");
        let mut store = CodegraphStore::open(&path, Uuid::new_v4()).unwrap();
        for index in 0..200 {
            store
                .connection
                .execute(
                    "INSERT INTO cg_index_runs(run_id,workspace_id,phase,started_at,source_digest,error)
                     VALUES (?1,'history','Failed','2026-01-01T00:00:00Z',?2,?3)",
                    params![Uuid::new_v4().to_string(), format!("{index:064x}"), "x".repeat(4096)],
                )
                .unwrap();
        }
        store
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        store
            .connection
            .execute("DELETE FROM cg_index_runs WHERE workspace_id='history'", [])
            .unwrap();
        assert!(store.compact_pruned_history(0).unwrap());
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "{after} bytes after vs {before} before");
    }

    #[test]
    fn compaction_defers_for_active_wal_reader() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codegraph.sqlite");
        let mut store = CodegraphStore::open(&path, Uuid::new_v4()).unwrap();
        let reader = rusqlite::Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT count(*) FROM cg_index_runs", [], |row| row.get(0))
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO cg_index_runs(run_id,workspace_id,phase,started_at,source_digest)
                 VALUES (?1,'history','Failed','2026-01-01T00:00:00Z',?2)",
                params![Uuid::new_v4().to_string(), "a".repeat(64)],
            )
            .unwrap();
        assert!(!store.compact_pruned_history(0).unwrap());
        reader.execute_batch("COMMIT").unwrap();
        assert!(store.compact_pruned_history(0).unwrap());
    }

    #[test]
    fn page_limit_refuses_growth_without_corrupting_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codegraph.sqlite");
        let project_id = Uuid::new_v4();
        let store = CodegraphStore::open(&path, project_id).unwrap();
        let pages: u64 = store
            .connection
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap();
        let page_size: u64 = store
            .connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        store.constrain_database_bytes(pages * page_size).unwrap();
        let write = store.connection.execute(
            "INSERT INTO cg_index_runs(run_id,workspace_id,phase,started_at,source_digest,error)
             VALUES (?1,'bounded','Failed','2026-01-01T00:00:00Z',?2,?3)",
            params![
                Uuid::new_v4().to_string(),
                "a".repeat(64),
                "x".repeat(1_000_000)
            ],
        );
        assert!(write.is_err());
        let reopened = CodegraphStore::open(&path, project_id).unwrap();
        let count: i64 = reopened
            .connection
            .query_row("SELECT count(*) FROM cg_index_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
