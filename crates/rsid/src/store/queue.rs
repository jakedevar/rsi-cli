//! Background queue persistence operations.

use super::Store;
use crate::error::Result;
use chrono::Utc;

/// Status values for background queue items.
pub const QUEUE_STATUS_PENDING: &str = "pending";
pub const QUEUE_STATUS_CLAIMED: &str = "claimed";
pub const QUEUE_STATUS_COMPLETED: &str = "completed";
pub const QUEUE_STATUS_FAILED: &str = "failed";

/// A row from the background_queue table.
#[derive(Debug, Clone)]
pub struct QueueItem {
    pub id: i64,
    pub work_unit_key: String,
    pub task_type: String,
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub payload: String,
    pub token_count: i64,
    pub status: String,
    pub priority: i32,
    pub attempts: i32,
    pub max_attempts: i32,
    pub error: Option<String>,
    pub created_at: String,
    pub claimed_at: Option<String>,
    pub completed_at: Option<String>,
}

/// Summary of accumulated tokens per work unit key for threshold gating.
#[derive(Debug, Clone)]
pub struct WorkUnitSummary {
    pub work_unit_key: String,
    pub task_type: String,
    pub total_tokens: i64,
    pub item_count: i64,
    pub oldest_id: i64,
}

/// Queue metrics for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct QueueMetrics {
    pub pending: i64,
    pub claimed: i64,
    pub completed: i64,
    pub failed: i64,
}

impl Store {
    /// Enqueue a new task. Returns the row ID.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_task(
        &self,
        work_unit_key: &str,
        task_type: &str,
        session_id: Option<&str>,
        project_id: Option<&str>,
        payload: &str,
        token_count: i64,
        priority: i32,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        self.conn.execute(
            "INSERT INTO background_queue
                (work_unit_key, task_type, session_id, project_id, payload,
                 token_count, status, priority, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
            rusqlite::params![
                work_unit_key,
                task_type,
                session_id,
                project_id,
                payload,
                token_count,
                priority,
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// List work unit summaries where accumulated tokens >= threshold
    /// and no items are currently claimed.
    pub fn list_eligible_work_units(&self, token_threshold: i64) -> Result<Vec<WorkUnitSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT work_unit_key, task_type,
                    SUM(token_count) as total_tokens,
                    COUNT(*) as item_count,
                    MIN(id) as oldest_id
             FROM background_queue
             WHERE status = 'pending'
             GROUP BY work_unit_key
             HAVING SUM(token_count) >= ?1
             ORDER BY MAX(priority) DESC, MIN(created_at) ASC",
        )?;
        let rows = stmt.query_map([token_threshold], |row| {
            Ok(WorkUnitSummary {
                work_unit_key: row.get(0)?,
                task_type: row.get(1)?,
                total_tokens: row.get(2)?,
                item_count: row.get(3)?,
                oldest_id: row.get(4)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Claim all pending items for a work unit key.
    /// Returns claimed items, or empty vec if another worker claimed them.
    pub fn claim_work_unit(&self, work_unit_key: &str) -> Result<Vec<QueueItem>> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let updated = self.conn.execute(
            "UPDATE background_queue
             SET status = 'claimed', claimed_at = ?1
             WHERE work_unit_key = ?2 AND status = 'pending'",
            rusqlite::params![now, work_unit_key],
        )?;
        if updated == 0 {
            return Ok(Vec::new());
        }
        self.get_items_by_work_unit(work_unit_key, QUEUE_STATUS_CLAIMED)
    }

    /// Mark all claimed items for a work unit as completed.
    pub fn complete_work_unit(&self, work_unit_key: &str) -> Result<usize> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let count = self.conn.execute(
            "UPDATE background_queue
             SET status = 'completed', completed_at = ?1
             WHERE work_unit_key = ?2 AND status = 'claimed'",
            rusqlite::params![now, work_unit_key],
        )?;
        Ok(count)
    }

    /// Mark all claimed items for a work unit as failed with error message.
    /// Increments attempt count. Items under max_attempts revert to pending.
    ///
    /// The three-statement sequence runs inside a single transaction so a
    /// crash between statements cannot leave a claimed item with an
    /// incremented attempt count but no status transition applied.
    pub fn fail_work_unit(&self, work_unit_key: &str, error: &str) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        // First, increment attempts and record error
        tx.execute(
            "UPDATE background_queue
             SET attempts = attempts + 1, error = ?1, claimed_at = NULL
             WHERE work_unit_key = ?2 AND status = 'claimed'",
            rusqlite::params![error, work_unit_key],
        )?;
        // Revert retryable items to pending
        let retried = tx.execute(
            "UPDATE background_queue
             SET status = 'pending'
             WHERE work_unit_key = ?1 AND status = 'claimed' AND attempts < max_attempts",
            [work_unit_key],
        )?;
        // Mark exhausted items as failed
        let failed = tx.execute(
            "UPDATE background_queue
             SET status = 'failed'
             WHERE work_unit_key = ?1 AND status = 'claimed' AND attempts >= max_attempts",
            [work_unit_key],
        )?;
        tx.commit()?;
        Ok(retried + failed)
    }

    /// Release stale claims older than `stale_secs` seconds.
    /// Returns the number of items released back to pending.
    pub fn release_stale_claims(&self, stale_secs: i64) -> Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::seconds(stale_secs))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let count = self.conn.execute(
            "UPDATE background_queue
             SET status = 'pending', claimed_at = NULL
             WHERE status = 'claimed' AND claimed_at < ?1",
            [cutoff],
        )?;
        Ok(count)
    }

    /// Get queue metrics for diagnostics.
    pub fn queue_metrics(&self) -> Result<QueueMetrics> {
        let pending: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM background_queue WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let claimed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM background_queue WHERE status = 'claimed'",
            [],
            |r| r.get(0),
        )?;
        let completed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM background_queue WHERE status = 'completed'",
            [],
            |r| r.get(0),
        )?;
        let failed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM background_queue WHERE status = 'failed'",
            [],
            |r| r.get(0),
        )?;
        Ok(QueueMetrics {
            pending,
            claimed,
            completed,
            failed,
        })
    }

    /// Purge completed items older than `retention_secs` seconds.
    pub fn purge_completed_items(&self, retention_secs: i64) -> Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::seconds(retention_secs))
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let count = self.conn.execute(
            "DELETE FROM background_queue
             WHERE status = 'completed' AND completed_at < ?1",
            [cutoff],
        )?;
        Ok(count)
    }

    /// Helper: get items by work unit key and status.
    fn get_items_by_work_unit(&self, work_unit_key: &str, status: &str) -> Result<Vec<QueueItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, work_unit_key, task_type, session_id, project_id,
                    payload, token_count, status, priority, attempts,
                    max_attempts, error, created_at, claimed_at, completed_at
             FROM background_queue
             WHERE work_unit_key = ?1 AND status = ?2
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![work_unit_key, status], |row| {
            Ok(QueueItem {
                id: row.get(0)?,
                work_unit_key: row.get(1)?,
                task_type: row.get(2)?,
                session_id: row.get(3)?,
                project_id: row.get(4)?,
                payload: row.get(5)?,
                token_count: row.get(6)?,
                status: row.get(7)?,
                priority: row.get(8)?,
                attempts: row.get(9)?,
                max_attempts: row.get(10)?,
                error: row.get(11)?,
                created_at: row.get(12)?,
                claimed_at: row.get(13)?,
                completed_at: row.get(14)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        // Store::open runs init_schema which applies all migrations including V28
        let store = Store::open(&db_path).unwrap();
        // Return the tempdir guard so it lives for the duration of the test and
        // is removed when the test ends. Leaking it here orphans the directory
        // in /tmp permanently.
        (dir, store)
    }

    #[test]
    fn test_enqueue_task() {
        let (_dir, store) = open_test_store();
        let id = store
            .enqueue_task(
                "extract_observations:proj1:sess1",
                "extract_observations",
                Some("sess1"),
                Some("proj1"),
                r#"{"content":"hello"}"#,
                512,
                0,
            )
            .unwrap();
        assert!(id > 0);

        // Verify the row exists
        let items = store
            .get_items_by_work_unit("extract_observations:proj1:sess1", QUEUE_STATUS_PENDING)
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert_eq!(items[0].task_type, "extract_observations");
        assert_eq!(items[0].session_id.as_deref(), Some("sess1"));
        assert_eq!(items[0].project_id.as_deref(), Some("proj1"));
        assert_eq!(items[0].token_count, 512);
        assert_eq!(items[0].status, QUEUE_STATUS_PENDING);
        assert_eq!(items[0].attempts, 0);
    }

    #[test]
    fn test_enqueue_returns_incrementing_ids() {
        let (_dir, store) = open_test_store();
        let id1 = store
            .enqueue_task("key1", "summarize", None, None, "{}", 100, 0)
            .unwrap();
        let id2 = store
            .enqueue_task("key1", "summarize", None, None, "{}", 200, 0)
            .unwrap();
        assert!(id2 > id1);
    }

    #[test]
    fn test_list_eligible_above_threshold() {
        let (_dir, store) = open_test_store();
        // Enqueue items totaling 1500 tokens (above threshold of 1024)
        store
            .enqueue_task("key:a", "extract_observations", None, None, "{}", 800, 0)
            .unwrap();
        store
            .enqueue_task("key:a", "extract_observations", None, None, "{}", 700, 0)
            .unwrap();

        let eligible = store.list_eligible_work_units(1024).unwrap();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].work_unit_key, "key:a");
        assert_eq!(eligible[0].total_tokens, 1500);
        assert_eq!(eligible[0].item_count, 2);
    }

    #[test]
    fn test_list_eligible_below_threshold() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:b", "summarize", None, None, "{}", 500, 0)
            .unwrap();

        let eligible = store.list_eligible_work_units(1024).unwrap();
        assert!(eligible.is_empty());
    }

    #[test]
    fn test_list_eligible_skips_claimed() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:c", "dream", None, None, "{}", 2000, 0)
            .unwrap();

        // Claim the work unit
        let claimed = store.claim_work_unit("key:c").unwrap();
        assert_eq!(claimed.len(), 1);

        // Now list eligible -- should be empty since items are claimed
        let eligible = store.list_eligible_work_units(0).unwrap();
        assert!(eligible.is_empty());
    }

    #[test]
    fn test_claim_work_unit() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:d", "reconcile", None, None, "{}", 100, 0)
            .unwrap();

        let claimed = store.claim_work_unit("key:d").unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].status, QUEUE_STATUS_CLAIMED);
        assert!(claimed[0].claimed_at.is_some());
    }

    #[test]
    fn test_claim_already_claimed() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:e", "dream", None, None, "{}", 100, 0)
            .unwrap();

        let first = store.claim_work_unit("key:e").unwrap();
        assert_eq!(first.len(), 1);

        let second = store.claim_work_unit("key:e").unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn test_complete_work_unit() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:f", "summarize", None, None, "{}", 100, 0)
            .unwrap();
        store.claim_work_unit("key:f").unwrap();

        let count = store.complete_work_unit("key:f").unwrap();
        assert_eq!(count, 1);

        let items = store
            .get_items_by_work_unit("key:f", QUEUE_STATUS_COMPLETED)
            .unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].completed_at.is_some());
    }

    #[test]
    fn test_fail_work_unit_with_retry() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:g", "dream", None, None, "{}", 100, 0)
            .unwrap();
        store.claim_work_unit("key:g").unwrap();

        let count = store.fail_work_unit("key:g", "timeout error").unwrap();
        assert_eq!(count, 1);

        // Item should be back to pending with attempt incremented
        let items = store
            .get_items_by_work_unit("key:g", QUEUE_STATUS_PENDING)
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].attempts, 1);
        assert_eq!(items[0].error.as_deref(), Some("timeout error"));
    }

    #[test]
    fn test_fail_work_unit_is_atomic_on_partial_failure() {
        // Proves the three UPDATEs in `fail_work_unit` run inside a single
        // transaction: if the LAST statement (marking exhausted items
        // 'failed') fails partway through, the FIRST statement (the
        // attempts increment) must not be left committed on its own. Before
        // the fix, each UPDATE was its own auto-committed statement, so a
        // failure here would leave the row with attempts incremented but
        // still status='claimed' -- exactly the stuck state described in
        // the defect report.
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:atomic", "dream", None, None, "{}", 100, 0)
            .unwrap();
        store.claim_work_unit("key:atomic").unwrap();

        // Push attempts to max_attempts - 1 so this fail_work_unit call
        // exhausts the item and takes the third (failed-status) branch.
        store
            .conn
            .execute(
                "UPDATE background_queue SET attempts = 4 WHERE work_unit_key = 'key:atomic'",
                [],
            )
            .unwrap();

        // Install a trigger that simulates a crash/error partway through
        // the sequence by rejecting the third UPDATE (status -> 'failed').
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER simulate_crash_on_fail
                 BEFORE UPDATE OF status ON background_queue
                 WHEN NEW.status = 'failed'
                 BEGIN
                    SELECT RAISE(ABORT, 'simulated crash mid fail_work_unit');
                 END;",
            )
            .unwrap();

        // The call must surface an error (the trigger aborts it) rather
        // than silently succeeding.
        let result = store.fail_work_unit("key:atomic", "boom");
        assert!(
            result.is_err(),
            "expected the simulated crash to propagate as an error"
        );

        // Atomicity assertion: since the whole sequence is one transaction,
        // the abort on the third statement must roll back the first
        // statement's attempts increment too. The row must be exactly as
        // it was before this call: attempts == 4 and status == 'claimed'.
        // Without the fix, this would fail: attempts would already be 5
        // (committed by the first standalone UPDATE) while status remained
        // 'claimed', reproducing the stuck-item defect.
        let (attempts, status): (i32, String) = store
            .conn
            .query_row(
                "SELECT attempts, status FROM background_queue WHERE work_unit_key = 'key:atomic'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            attempts, 4,
            "attempts increment must roll back with the rest of the transaction"
        );
        assert_eq!(
            status, QUEUE_STATUS_CLAIMED,
            "status must remain claimed, not be left stuck mid-transition"
        );
    }

    #[test]
    fn test_fail_work_unit_exhausted() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:h", "dream", None, None, "{}", 100, 0)
            .unwrap();

        // Exhaust all attempts (default max_attempts = 5)
        for i in 0..5 {
            store.claim_work_unit("key:h").unwrap();
            store
                .fail_work_unit("key:h", &format!("error #{}", i + 1))
                .unwrap();
        }

        // After 5 failures, item should be in failed status
        let pending = store
            .get_items_by_work_unit("key:h", QUEUE_STATUS_PENDING)
            .unwrap();
        let failed = store
            .get_items_by_work_unit("key:h", QUEUE_STATUS_FAILED)
            .unwrap();
        assert!(pending.is_empty());
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].attempts, 5);
    }

    #[test]
    fn test_release_stale_claims() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:i", "reconcile", None, None, "{}", 100, 0)
            .unwrap();
        store.claim_work_unit("key:i").unwrap();

        // Manually set claimed_at to a very old timestamp
        store
            .conn
            .execute(
                "UPDATE background_queue SET claimed_at = '2020-01-01T00:00:00.000000000Z' WHERE work_unit_key = 'key:i'",
                [],
            )
            .unwrap();

        let released = store.release_stale_claims(300).unwrap();
        assert_eq!(released, 1);

        let items = store
            .get_items_by_work_unit("key:i", QUEUE_STATUS_PENDING)
            .unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn test_queue_metrics() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:j1", "dream", None, None, "{}", 100, 0)
            .unwrap();
        store
            .enqueue_task("key:j2", "dream", None, None, "{}", 200, 0)
            .unwrap();
        store
            .enqueue_task("key:j3", "dream", None, None, "{}", 300, 0)
            .unwrap();

        // Claim one
        store.claim_work_unit("key:j1").unwrap();
        // Complete one
        store.claim_work_unit("key:j2").unwrap();
        store.complete_work_unit("key:j2").unwrap();

        let metrics = store.queue_metrics().unwrap();
        assert_eq!(metrics.pending, 1);
        assert_eq!(metrics.claimed, 1);
        assert_eq!(metrics.completed, 1);
        assert_eq!(metrics.failed, 0);
    }

    #[test]
    fn test_purge_completed_items() {
        let (_dir, store) = open_test_store();
        store
            .enqueue_task("key:k", "reconcile", None, None, "{}", 100, 0)
            .unwrap();
        store.claim_work_unit("key:k").unwrap();
        store.complete_work_unit("key:k").unwrap();

        // Set completed_at to a very old timestamp
        store
            .conn
            .execute(
                "UPDATE background_queue SET completed_at = '2020-01-01T00:00:00.000000000Z' WHERE work_unit_key = 'key:k'",
                [],
            )
            .unwrap();

        let purged = store.purge_completed_items(86400).unwrap();
        assert_eq!(purged, 1);

        let metrics = store.queue_metrics().unwrap();
        assert_eq!(metrics.completed, 0);
    }

    #[test]
    fn test_work_unit_grouping() {
        let (_dir, store) = open_test_store();
        // Enqueue multiple items with same work unit key
        store
            .enqueue_task(
                "key:group",
                "extract_observations",
                Some("s1"),
                None,
                "{}",
                500,
                0,
            )
            .unwrap();
        store
            .enqueue_task(
                "key:group",
                "extract_observations",
                Some("s1"),
                None,
                "{}",
                600,
                0,
            )
            .unwrap();

        // Claim should return both items
        let claimed = store.claim_work_unit("key:group").unwrap();
        assert_eq!(claimed.len(), 2);

        // Complete should affect both
        let count = store.complete_work_unit("key:group").unwrap();
        assert_eq!(count, 2);
    }
}
