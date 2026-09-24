//! V46: `chain_iterations` table — per-iteration metadata for `master_improve` chains.

use chrono::{SecondsFormat, Utc};
use rsi_common::types::{ChainIteration, HaltReason};
use rusqlite::{Row, params};
use uuid::Uuid;

use super::Store;
use crate::error::{DaemonError, Result};

impl Store {
    /// Insert a fresh chain iteration row (`halt_reason` = NULL, `ended_at` = NULL).
    ///
    /// # Errors
    /// Returns a [`DaemonError::Database`] if the SQLite INSERT fails (e.g., PK conflict).
    pub fn insert_chain_iteration(&self, iter: &ChainIteration) -> Result<()> {
        self.conn.execute(
            "INSERT INTO chain_iterations
              (chain_id, iteration_index, parent_execution_id, child_execution_id,
               halt_reason, goal_text, refined_goal_text, token_count,
               pre_failure_count, post_failure_count, cap, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)",
            params![
                iter.chain_id.to_string(),
                iter.iteration_index,
                iter.parent_execution_id.map(|u| u.to_string()),
                iter.child_execution_id.to_string(),
                iter.goal_text,
                iter.refined_goal_text,
                iter.token_count,
                iter.pre_failure_count,
                iter.post_failure_count,
                iter.cap,
                iter.started_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(())
    }

    /// Mark an iteration's terminal outcome.
    ///
    /// # Errors
    /// Returns [`DaemonError::Store`] if `HaltReason` serialization fails, or
    /// [`DaemonError::Database`] if the SQLite UPDATE fails.
    pub fn update_chain_iteration_outcome(
        &self,
        chain_id: Uuid,
        iteration_index: u32,
        halt_reason: &HaltReason,
        post_failure_count: Option<u32>,
        token_count: Option<u64>,
    ) -> Result<()> {
        let halt_json = serde_json::to_string(halt_reason).map_err(|e| {
            DaemonError::Store(format!("serialize HaltReason for chain_iterations: {e}"))
        })?;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        self.conn.execute(
            "UPDATE chain_iterations
             SET halt_reason = ?1, post_failure_count = ?2, token_count = ?3, ended_at = ?4
             WHERE chain_id = ?5 AND iteration_index = ?6",
            params![
                halt_json,
                post_failure_count,
                token_count,
                now,
                chain_id.to_string(),
                iteration_index,
            ],
        )?;
        Ok(())
    }

    /// List all iterations of a chain in ascending `iteration_index` order.
    ///
    /// # Errors
    /// Returns [`DaemonError::Database`] if the SELECT or row mapping fails.
    pub fn list_chain_iterations(&self, chain_id: Uuid) -> Result<Vec<ChainIteration>> {
        let mut stmt = self.conn.prepare(
            "SELECT chain_id, iteration_index, parent_execution_id, child_execution_id,
                    halt_reason, goal_text, refined_goal_text, token_count,
                    pre_failure_count, post_failure_count, cap, started_at, ended_at
             FROM chain_iterations
             WHERE chain_id = ?1
             ORDER BY iteration_index ASC",
        )?;
        let rows = stmt.query_map(params![chain_id.to_string()], map_chain_iteration_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Look up the `chain_id` + `iteration_index` for a workflow `execution_id`.
    /// Returns `None` if the `execution_id` doesn't belong to a chain.
    ///
    /// # Errors
    /// Returns [`DaemonError::Database`] if the SELECT fails or the stored UUID is malformed.
    pub fn get_chain_for_execution(&self, execution_id: Uuid) -> Result<Option<(Uuid, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT chain_id, iteration_index FROM chain_iterations
             WHERE child_execution_id = ?1
             LIMIT 1",
        )?;
        let mut rows = stmt.query(params![execution_id.to_string()])?;
        if let Some(row) = rows.next()? {
            let chain_id_s: String = row.get(0)?;
            let iter_idx: u32 = row.get(1)?;
            let chain_id = Uuid::parse_str(&chain_id_s)
                .map_err(|e| DaemonError::Store(format!("invalid chain_id uuid: {e}")))?;
            Ok(Some((chain_id, iter_idx)))
        } else {
            Ok(None)
        }
    }

    /// List active chains (chains whose latest row has `halt_reason` IS NULL).
    /// Returns `(chain_id, max_iteration_index)`.
    ///
    /// # Errors
    /// Returns [`DaemonError::Database`] if the SELECT fails or a stored UUID is malformed.
    pub fn list_active_chains(&self) -> Result<Vec<(Uuid, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT chain_id, MAX(iteration_index) FROM chain_iterations
             WHERE halt_reason IS NULL
             GROUP BY chain_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let s: String = row.get(0)?;
            let idx: u32 = row.get(1)?;
            Ok((s, idx))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (s, idx) = r?;
            let chain_id = Uuid::parse_str(&s)
                .map_err(|e| DaemonError::Store(format!("invalid chain_id uuid: {e}")))?;
            out.push((chain_id, idx));
        }
        Ok(out)
    }
}

fn map_chain_iteration_row(row: &Row<'_>) -> rusqlite::Result<ChainIteration> {
    let chain_id_s: String = row.get(0)?;
    let parent_s: Option<String> = row.get(2)?;
    let child_s: String = row.get(3)?;
    let halt_json: Option<String> = row.get(4)?;
    let started_s: String = row.get(11)?;
    let ended_s: Option<String> = row.get(12)?;

    let halt_reason = halt_json
        .map(|j| serde_json::from_str::<HaltReason>(&j))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?;

    Ok(ChainIteration {
        chain_id: Uuid::parse_str(&chain_id_s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?,
        iteration_index: row.get(1)?,
        parent_execution_id: parent_s
            .map(|s| Uuid::parse_str(&s))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
        child_execution_id: Uuid::parse_str(&child_s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?,
        halt_reason,
        goal_text: row.get(5)?,
        refined_goal_text: row.get(6)?,
        token_count: row.get(7)?,
        pre_failure_count: row.get(8)?,
        post_failure_count: row.get(9)?,
        cap: row.get(10)?,
        started_at: chrono::DateTime::parse_from_rfc3339(&started_s)
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    11,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
            .with_timezone(&Utc),
        ended_at: ended_s
            .map(|s| chrono::DateTime::parse_from_rfc3339(&s).map(|dt| dt.with_timezone(&Utc)))
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use chrono::Utc;

    fn make_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let store = Store::open(&db_path).unwrap();
        (dir, store)
    }

    fn make_iter(chain_id: Uuid, idx: u32) -> ChainIteration {
        ChainIteration {
            chain_id,
            iteration_index: idx,
            parent_execution_id: if idx == 0 { None } else { Some(Uuid::new_v4()) },
            child_execution_id: Uuid::new_v4(),
            halt_reason: None,
            goal_text: "initial goal".to_string(),
            refined_goal_text: Some("refined".to_string()),
            token_count: Some(1234),
            pre_failure_count: Some(2),
            post_failure_count: None,
            cap: 5,
            started_at: Utc::now(),
            ended_at: None,
        }
    }

    fn round_trip_halt(reason: &HaltReason) {
        let (_dir, store) = make_store();
        let chain_id = Uuid::new_v4();
        let iter = make_iter(chain_id, 0);
        let exec_id = iter.child_execution_id;
        store.insert_chain_iteration(&iter).unwrap();
        store
            .update_chain_iteration_outcome(chain_id, 0, reason, Some(3), Some(9999))
            .unwrap();
        let rows = store.list_chain_iterations(chain_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].halt_reason.as_ref(), Some(reason));
        assert_eq!(rows[0].post_failure_count, Some(3));
        assert_eq!(rows[0].token_count, Some(9999));
        assert!(rows[0].ended_at.is_some());

        // get_chain_for_execution returns Some for inserted exec_id
        let lookup = store.get_chain_for_execution(exec_id).unwrap();
        assert_eq!(lookup, Some((chain_id, 0)));
    }

    #[test]
    fn round_trip_halt_done() {
        round_trip_halt(&HaltReason::Done);
    }

    #[test]
    fn round_trip_halt_cap() {
        round_trip_halt(&HaltReason::Cap);
    }

    #[test]
    fn round_trip_halt_regression() {
        round_trip_halt(&HaltReason::Regression { pre: 1, post: 4 });
    }

    #[test]
    fn round_trip_halt_stop_file() {
        round_trip_halt(&HaltReason::StopFile);
    }

    #[test]
    fn round_trip_halt_judge_blocked() {
        round_trip_halt(&HaltReason::JudgeBlocked("missing data".to_string()));
    }

    #[test]
    fn round_trip_halt_judge_malformed() {
        round_trip_halt(&HaltReason::JudgeMalformed);
    }

    #[test]
    fn round_trip_halt_error() {
        round_trip_halt(&HaltReason::Error("daemon crash".to_string()));
    }

    #[test]
    fn list_active_chains_filters_null_halt_rows() {
        let (_dir, store) = make_store();
        let chain_a = Uuid::new_v4();
        let chain_b = Uuid::new_v4();

        // Chain A: 2 iterations, the 2nd is still active (halt NULL).
        let mut a0 = make_iter(chain_a, 0);
        a0.parent_execution_id = None;
        store.insert_chain_iteration(&a0).unwrap();
        store
            .update_chain_iteration_outcome(chain_a, 0, &HaltReason::Done, None, None)
            .unwrap();
        let a1 = make_iter(chain_a, 1);
        store.insert_chain_iteration(&a1).unwrap();

        // Chain B: 1 iteration, terminal.
        let b0 = make_iter(chain_b, 0);
        store.insert_chain_iteration(&b0).unwrap();
        store
            .update_chain_iteration_outcome(chain_b, 0, &HaltReason::Cap, None, None)
            .unwrap();

        let active = store.list_active_chains().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0], (chain_a, 1));
    }

    #[test]
    fn get_chain_for_execution_returns_none_for_unknown() {
        let (_dir, store) = make_store();
        let unknown = Uuid::new_v4();
        let result = store.get_chain_for_execution(unknown).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn list_chain_iterations_orders_ascending() {
        let (_dir, store) = make_store();
        let chain_id = Uuid::new_v4();
        // Insert out of order
        store
            .insert_chain_iteration(&make_iter(chain_id, 2))
            .unwrap();
        store
            .insert_chain_iteration(&make_iter(chain_id, 0))
            .unwrap();
        store
            .insert_chain_iteration(&make_iter(chain_id, 1))
            .unwrap();

        let rows = store.list_chain_iterations(chain_id).unwrap();
        let indices: Vec<u32> = rows.iter().map(|r| r.iteration_index).collect();
        assert_eq!(indices, vec![0, 1, 2]);
    }
}
