//! Task processor trait and default no-op implementation.

use super::types::TaskType;
use crate::error::Result;
use crate::store::queue::QueueItem;

/// Trait for processing claimed queue items.
/// Implementors handle specific task types (observation extraction, summarization, etc.).
#[async_trait::async_trait]
pub trait TaskProcessor: Send + Sync {
    /// Process a batch of claimed items for a single work unit.
    /// Returns Ok(()) on success (items will be marked completed).
    /// Returns Err on failure (items will be retried or marked failed).
    async fn process(&self, task_type: TaskType, items: &[QueueItem]) -> Result<()>;

    /// Whether this processor handles the given task type.
    fn handles(&self, task_type: TaskType) -> bool;
}

/// No-op processor that logs and completes all tasks.
/// Used as the default until real processors are implemented.
pub struct NoOpProcessor;

#[async_trait::async_trait]
impl TaskProcessor for NoOpProcessor {
    async fn process(&self, task_type: TaskType, items: &[QueueItem]) -> Result<()> {
        tracing::debug!(
            task_type = %task_type,
            item_count = items.len(),
            "queue: no-op processor completed work unit"
        );
        Ok(())
    }

    fn handles(&self, _task_type: TaskType) -> bool {
        true
    }
}
