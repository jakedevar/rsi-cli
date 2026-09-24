use std::time::Duration;

use rsi_codegraph::{
    ReadySnapshot,
    lifecycle::{DurableIndexStatus, IndexLifecyclePhase},
};
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
        }
    }
}
