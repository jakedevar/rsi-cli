use std::time::Duration;

use chrono::{DateTime, Utc};
use rsi_codegraph::{
    ReadySnapshot,
    lifecycle::{DurableIndexStatus, IndexLifecyclePhase},
};
use rsi_common::codegraph::CodegraphHealthNoticeV1;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPhase {
    Queued,
    Building,
    Ready,
    Stale,
    Degraded,
    Failed,
    Recovering,
}

/// Current process view, reconstructed from the project-bound store on startup.
#[derive(Debug, Clone)]
pub struct IndexStatus {
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub phase: IndexPhase,
    pub ready: Option<ReadySnapshot>,
    pub files_discovered: usize,
    pub bytes_discovered: usize,
    pub files_hashed: usize,
    pub files_reused: usize,
    pub files_extracted: usize,
    pub changed_paths: usize,
    pub staged_files: usize,
    pub last_published: bool,
    pub overflow_count: u64,
    pub pending_rescan: bool,
    pub rescan_reason: Option<String>,
    pub last_error: Option<String>,
    pub last_duration: Option<Duration>,
    /// When the graph last left a good state, or was first seen bad: the last
    /// published head's time for a restored Degraded/Failed row, else the
    /// first failure this process saw. `None` while healthy (#2246).
    pub unhealthy_since: Option<DateTime<Utc>>,
    /// Real (non-transient) failed runs since `unhealthy_since`.
    pub failed_runs: u64,
    /// First real failed run of the current failure episode; cleared by the
    /// next success. Identifies the episode an owner notice is sent for
    /// (#2454). A restored Degraded/Failed row starts without one: the first
    /// real failure after boot opens an episode.
    pub episode_started_at: Option<DateTime<Utc>>,
}

/// One workspace's current failure episode (#2454).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureEpisode {
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub phase: &'static str,
    pub started_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub failed_runs: u64,
}

impl IndexStatus {
    pub fn from_durable(project_id: Uuid, durable: DurableIndexStatus) -> Self {
        let phase = match durable.phase {
            IndexLifecyclePhase::Queued => IndexPhase::Queued,
            IndexLifecyclePhase::Building => IndexPhase::Building,
            IndexLifecyclePhase::Ready => IndexPhase::Ready,
            IndexLifecyclePhase::Stale => IndexPhase::Stale,
            IndexLifecyclePhase::Degraded => IndexPhase::Degraded,
            IndexLifecyclePhase::Failed => IndexPhase::Failed,
            IndexLifecyclePhase::Recovering => IndexPhase::Recovering,
        };
        let unhealthy_since =
            matches!(phase, IndexPhase::Degraded | IndexPhase::Failed).then(|| {
                durable
                    .last_success_at
                    .as_deref()
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map_or_else(Utc::now, |at| at.with_timezone(&Utc))
            });
        Self {
            project_id,
            workspace_id: durable.workspace_id,
            phase,
            ready: durable.ready,
            files_discovered: durable.metrics.files_discovered,
            bytes_discovered: durable.metrics.bytes_discovered,
            files_hashed: durable.metrics.files_hashed,
            files_reused: durable.metrics.files_reused,
            files_extracted: durable.metrics.files_extracted,
            changed_paths: durable.metrics.changed_paths,
            staged_files: durable.metrics.staged_files,
            last_published: false,
            overflow_count: durable.metrics.overflow_count,
            pending_rescan: durable.metrics.pending_rescan,
            rescan_reason: durable.rescan_reason,
            last_error: durable.last_error,
            last_duration: Some(Duration::from_millis(durable.metrics.duration_ms)),
            unhealthy_since,
            failed_runs: 0,
            episode_started_at: None,
        }
    }

    pub fn new(project_id: Uuid, workspace_id: Uuid) -> Self {
        Self {
            project_id,
            workspace_id,
            phase: IndexPhase::Queued,
            ready: None,
            files_discovered: 0,
            bytes_discovered: 0,
            files_hashed: 0,
            files_reused: 0,
            files_extracted: 0,
            changed_paths: 0,
            staged_files: 0,
            last_published: false,
            overflow_count: 0,
            pending_rescan: false,
            rescan_reason: None,
            last_error: None,
            last_duration: None,
            unhealthy_since: None,
            failed_runs: 0,
            episode_started_at: None,
        }
    }

    /// Record a finished run. A success clears the unhealthy clock; any
    /// failure starts it (once). Only a `real` failure counts toward
    /// `failed_runs`, so a superseded or reader-deferred run is not a streak.
    pub fn note_run(&mut self, failed: bool, real: bool, now: DateTime<Utc>) {
        if !failed {
            self.unhealthy_since = None;
            self.failed_runs = 0;
            self.episode_started_at = None;
            return;
        }
        self.unhealthy_since.get_or_insert(now);
        if real {
            self.failed_runs = self.failed_runs.saturating_add(1);
            self.episode_started_at.get_or_insert(now);
        }
    }

    /// The current failure episode, from its first real failed run. Unlike
    /// [`Self::unhealthy_notice`] it has no age threshold: it is the owner's
    /// first signal, not the day-old escalation (#2454).
    #[must_use]
    pub fn failure_episode(&self) -> Option<FailureEpisode> {
        if !matches!(self.phase, IndexPhase::Degraded | IndexPhase::Failed) {
            return None;
        }
        Some(FailureEpisode {
            project_id: self.project_id,
            workspace_id: self.workspace_id,
            phase: if self.phase == IndexPhase::Degraded {
                "degraded"
            } else {
                "failed"
            },
            started_at: self.episode_started_at?,
            last_error: self.last_error.clone(),
            failed_runs: self.failed_runs,
        })
    }

    /// The health notice for this workspace once it has been Degraded or
    /// Failed for at least `threshold_secs`.
    #[must_use]
    pub fn unhealthy_notice(
        &self,
        now: DateTime<Utc>,
        threshold_secs: i64,
    ) -> Option<CodegraphHealthNoticeV1> {
        if !matches!(self.phase, IndexPhase::Degraded | IndexPhase::Failed) {
            return None;
        }
        let since = self.unhealthy_since?;
        if (now - since).num_seconds() < threshold_secs {
            return None;
        }
        Some(CodegraphHealthNoticeV1 {
            project_id: self.project_id,
            workspace_id: self.workspace_id,
            phase: if self.phase == IndexPhase::Degraded {
                "degraded"
            } else {
                "failed"
            }
            .into(),
            unhealthy_since: since.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            last_error: self.last_error.clone(),
            failed_runs: self.failed_runs,
        })
    }
}
