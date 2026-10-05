//! #1059: the satellite-side write-ahead record of a hub delivery.
//!
//! `deliver_hub_message` writes a `pending` attempt BEFORE the effect (the
//! continuation of the idle target session) and settles it `delivered` after.
//! A crash or store failure between the two leaves the row `pending`, and a
//! `pending` row is an uncertain delivery: a retry of the same `message_id`
//! reports `uncertain` and never delivers again (at most once, #945 rule).
//! Only a definite non-effect (the target went busy) settles `not_delivered`,
//! which a later retry may attempt again. `delivered` rows are immutable.
//!
//! Rows written by the V145 ledger (`satellite_inbound_deliveries`) remain
//! authoritative `delivered` receipts; V145 is not edited.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: satellite-inbound-attempts-migration
/// Provisional schema version of the attempts catalog; the lander renumbers it.
pub(crate) const SATELLITE_INBOUND_ATTEMPTS_SCHEMA_VERSION: i32 = 147;

const CATALOG: &str = "CREATE TABLE satellite_inbound_attempts (
    message_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(message_id)),
    hub_installation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(hub_installation_id)),
    sender_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(sender_session_id)),
    target_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(target_session_id)),
    payload_digest TEXT NOT NULL CHECK(length(payload_digest)=64),
    state TEXT NOT NULL CHECK(state IN ('pending','delivered','not_delivered')),
    started_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(started_at)),
    settled_at TEXT CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
    CHECK((state='pending' AND settled_at IS NULL) OR (state!='pending' AND settled_at IS NOT NULL))
);
CREATE TRIGGER satellite_inbound_attempts_no_delete BEFORE DELETE ON satellite_inbound_attempts BEGIN SELECT RAISE(ABORT,'satellite inbound attempts are append only'); END;
CREATE TRIGGER satellite_inbound_attempts_delivered_immutable BEFORE UPDATE ON satellite_inbound_attempts WHEN OLD.state='delivered' BEGIN SELECT RAISE(ABORT,'delivered satellite inbound attempts are immutable'); END;
CREATE TRIGGER satellite_inbound_attempts_identity_immutable BEFORE UPDATE ON satellite_inbound_attempts
WHEN NEW.message_id!=OLD.message_id OR NEW.hub_installation_id!=OLD.hub_installation_id
  OR NEW.sender_session_id!=OLD.sender_session_id OR NEW.target_session_id!=OLD.target_session_id
  OR NEW.payload_digest!=OLD.payload_digest OR NEW.started_at!=OLD.started_at
BEGIN SELECT RAISE(ABORT,'satellite inbound attempt identity is immutable'); END;";

/// Catalog objects, for the fixture rewind and presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 4] = [
    ("table", "satellite_inbound_attempts"),
    ("trigger", "satellite_inbound_attempts_no_delete"),
    ("trigger", "satellite_inbound_attempts_delivered_immutable"),
    ("trigger", "satellite_inbound_attempts_identity_immutable"),
];

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "satellite inbound attempts require V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: satellite-inbound-attempts-migration

/// What the satellite knows about one hub `message_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundState {
    /// The effect happened and was recorded.
    Delivered,
    /// An attempt was recorded before the effect and never settled: whether
    /// the effect happened is unknown.
    Pending,
    /// The attempt is known not to have had an effect; it may be retried.
    NotDelivered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundReceipt {
    pub target_session_id: Uuid,
    pub payload_digest: String,
    pub state: InboundState,
}

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

impl Store {
    /// The receipt for `message_id`: a V145 ledger row (delivered) or an
    /// attempt row, if any.
    pub fn satellite_inbound_receipt(&self, message_id: Uuid) -> Result<Option<InboundReceipt>> {
        if let Some((target_session_id, payload_digest)) =
            self.satellite_inbound_delivery(message_id)?
        {
            return Ok(Some(InboundReceipt {
                target_session_id,
                payload_digest,
                state: InboundState::Delivered,
            }));
        }
        let row = self
            .conn
            .query_row(
                "SELECT target_session_id,payload_digest,state FROM satellite_inbound_attempts
                 WHERE message_id=?1",
                [message_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(target, payload_digest, state)| {
            Ok(InboundReceipt {
                target_session_id: Uuid::parse_str(&target)
                    .map_err(|_| DaemonError::Store("invalid satellite message id".into()))?,
                payload_digest,
                state: match state.as_str() {
                    "delivered" => InboundState::Delivered,
                    "not_delivered" => InboundState::NotDelivered,
                    _ => InboundState::Pending,
                },
            })
        })
        .transpose()
    }

    /// Record (or re-open, after a definite non-effect) the `pending` attempt
    /// for `message_id`. Must run before the effect. Returns false when a
    /// live or delivered row already holds the id.
    pub fn begin_satellite_inbound_attempt(
        &self,
        message_id: Uuid,
        hub_installation_id: Uuid,
        sender_session_id: Uuid,
        target_session_id: Uuid,
        payload: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO satellite_inbound_attempts(message_id,hub_installation_id,
                sender_session_id,target_session_id,payload_digest,state,started_at)
             VALUES(?1,?2,?3,?4,?5,'pending',?6)
             ON CONFLICT(message_id) DO UPDATE SET state='pending',settled_at=NULL
             WHERE state='not_delivered'",
            params![
                message_id.to_string(),
                hub_installation_id.to_string(),
                sender_session_id.to_string(),
                target_session_id.to_string(),
                super::satellite_dispatch::inbound_payload_digest(payload),
                stamp(now),
            ],
        )?;
        Ok(changed == 1)
    }

    /// Settle a `pending` attempt as `delivered` or `not_delivered`.
    pub fn settle_satellite_inbound_attempt(
        &self,
        message_id: Uuid,
        delivered: bool,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE satellite_inbound_attempts SET state=?2,settled_at=?3
             WHERE message_id=?1 AND state='pending'",
            params![
                message_id.to_string(),
                if delivered {
                    "delivered"
                } else {
                    "not_delivered"
                },
                stamp(now),
            ],
        )?;
        Ok(changed == 1)
    }
}
