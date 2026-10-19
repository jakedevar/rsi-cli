//! #2528: the durable record a bounded shutdown leaves for the next boot.
//!
//! Model-call settlement waits for every live producer to quiesce before the
//! daemon exits. A producer that never finishes (a turn the daemon itself
//! refuses to serve) must not hold the restart forever, so past a deadline the
//! shutdown records the invocations that were still running here and exits.
//! The next boot reconciles them (`reconcile_running_model_invocations`) and
//! then closes this record, so an operator can see which invocations a
//! shutdown abandoned and that the boot settled them.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `daemon_settings` key of the record.
pub const UNSETTLED_MODEL_INVOCATIONS_KEY: &str = "shutdown.unsettled_model_invocations";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsettledModelInvocationsRecord {
    /// The incarnation that gave up waiting.
    pub boot_id: Uuid,
    pub recorded_at: String,
    pub invocation_ids: Vec<Uuid>,
    /// Set by the boot that reconciled the invocations.
    #[serde(default)]
    pub recovered_at: Option<String>,
}

/// One recorded invocation as the recovering boot found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredUnsettledInvocation {
    pub invocation_id: Uuid,
    /// `None` when the row no longer exists.
    pub status: Option<String>,
}

impl Store {
    /// Admitted invocations a provider turn still owns.
    pub fn running_model_invocation_ids(&self) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT id FROM model_invocations
             WHERE admission_status = 'admitted'
               AND status IN ('running', 'cancellation_requested')
             ORDER BY created_at, id",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|id| {
                Uuid::parse_str(&id).map_err(|error| {
                    DaemonError::Store(format!("invalid running invocation id {id}: {error}"))
                })
            })
            .collect()
    }

    /// Record the invocations still running when a shutdown stops waiting for
    /// them. Returns the recorded ids (empty records nothing).
    pub fn record_unsettled_model_invocations(&self, boot_id: Uuid) -> Result<Vec<Uuid>> {
        let invocation_ids = self.running_model_invocation_ids()?;
        if invocation_ids.is_empty() {
            return Ok(invocation_ids);
        }
        let record = UnsettledModelInvocationsRecord {
            boot_id,
            recorded_at: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
            invocation_ids: invocation_ids.clone(),
            recovered_at: None,
        };
        let value = serde_json::to_string(&record)
            .map_err(|error| DaemonError::Store(format!("unsettled record: {error}")))?;
        self.set_daemon_setting(UNSETTLED_MODEL_INVOCATIONS_KEY, &value)?;
        Ok(invocation_ids)
    }

    /// The record a previous incarnation left, if it is still open.
    pub fn open_unsettled_model_invocations(
        &self,
    ) -> Result<Option<UnsettledModelInvocationsRecord>> {
        let Some(raw) = self.get_daemon_setting(UNSETTLED_MODEL_INVOCATIONS_KEY)? else {
            return Ok(None);
        };
        let record: UnsettledModelInvocationsRecord = serde_json::from_str(&raw)
            .map_err(|error| DaemonError::Store(format!("unsettled record unreadable: {error}")))?;
        Ok(record.recovered_at.is_none().then_some(record))
    }

    /// Boot recovery. Call it after `reconcile_running_model_invocations`: it
    /// reports each recorded invocation's current status, and closes the record
    /// only when none of them is still running. A still-running one stays open
    /// (the next boot reports it again) because nothing has settled it.
    pub fn recover_recorded_unsettled_model_invocations(
        &self,
    ) -> Result<Vec<RecoveredUnsettledInvocation>> {
        let Some(mut record) = self.open_unsettled_model_invocations()? else {
            return Ok(Vec::new());
        };
        let mut recovered = Vec::with_capacity(record.invocation_ids.len());
        for invocation_id in &record.invocation_ids {
            let status = self
                .conn
                .query_row(
                    "SELECT status FROM model_invocations WHERE id = ?1",
                    params![invocation_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            recovered.push(RecoveredUnsettledInvocation {
                invocation_id: *invocation_id,
                status,
            });
        }
        let still_running = recovered.iter().any(|entry| {
            matches!(
                entry.status.as_deref(),
                Some("running" | "cancellation_requested")
            )
        });
        if !still_running {
            record.recovered_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true));
            let value = serde_json::to_string(&record)
                .map_err(|error| DaemonError::Store(format!("unsettled record: {error}")))?;
            self.set_daemon_setting(UNSETTLED_MODEL_INVOCATIONS_KEY, &value)?;
        }
        Ok(recovered)
    }
}
