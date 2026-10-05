//! #1017 slice 3: durable state for queued hub-manager delivery to satellites.
//!
//! Hub side: `satellite_messages` (the outbox, separate from `agent_messages`
//! so the local delivery state machine is untouched) and
//! `satellite_peer_scope` (operator-declared reachable remote sessions).
//! Satellite side: `satellite_inbound_policy` (operator-set allowlists) and
//! `satellite_inbound_deliveries` (the `message_id` replay ledger).
//!
//! Acceptance is one IMMEDIATE transaction; every refusal that could reveal
//! whether a peer or session exists is the same `target_not_authorized`.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::satellite::SatelliteUuidV1;
use rsi_common::satellite_dispatch::{
    AgentSendSatelliteMessageReceiptV1, AgentSendSatelliteMessageRequestV1,
    SATELLITE_MESSAGE_DEFAULT_TTL_SECS, SATELLITE_MESSAGE_EXPIRY_INVALID,
    SATELLITE_MESSAGE_KEY_CONFLICT, SATELLITE_MESSAGE_MAX_QUEUED, SATELLITE_MESSAGE_MAX_TTL_SECS,
    SATELLITE_MESSAGE_QUEUE_FULL, SATELLITE_SCOPE_MAX_SESSIONS, SATELLITE_TARGET_NOT_AUTHORIZED,
    SatelliteInboundPolicyV1,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: satellite-dispatch-migration
/// Provisional schema version of the dispatch catalog; the lander renumbers it.
pub(crate) const SATELLITE_DISPATCH_SCHEMA_VERSION: i32 = 145;

const CATALOG: &str = "CREATE TABLE satellite_peer_scope (
    peer_id TEXT NOT NULL REFERENCES satellite_peers(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(peer_id)),
    remote_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(remote_session_id)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    PRIMARY KEY(peer_id, remote_session_id)
);
CREATE TABLE satellite_messages (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    owner_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(owner_session_id)) REFERENCES sessions(id) ON DELETE RESTRICT,
    peer_id TEXT NOT NULL REFERENCES satellite_peers(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(peer_id)),
    remote_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(remote_session_id)),
    idempotency_digest TEXT NOT NULL CHECK(length(idempotency_digest)=64),
    request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=64),
    payload TEXT NOT NULL CHECK(length(CAST(payload AS BLOB)) BETWEEN 1 AND 16384),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    expires_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(expires_at)),
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','sent','failed','expired')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count>=0),
    safe_error_class TEXT CHECK(safe_error_class IS NULL OR length(safe_error_class) BETWEEN 1 AND 64),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    settled_at TEXT CHECK(settled_at IS NULL OR rsi_rfc3339_nanos_is_canonical(settled_at)),
    UNIQUE(owner_session_id, idempotency_digest),
    CHECK((state='queued' AND settled_at IS NULL) OR (state!='queued' AND settled_at IS NOT NULL))
);
CREATE INDEX satellite_messages_by_state ON satellite_messages(state, peer_id, created_at);
CREATE TRIGGER satellite_messages_no_delete BEFORE DELETE ON satellite_messages BEGIN SELECT RAISE(ABORT,'satellite messages are append only'); END;
CREATE TRIGGER satellite_messages_settled_immutable BEFORE UPDATE ON satellite_messages WHEN OLD.state!='queued' BEGIN SELECT RAISE(ABORT,'settled satellite messages are immutable'); END;
CREATE TRIGGER satellite_messages_identity_immutable BEFORE UPDATE ON satellite_messages
WHEN NEW.id!=OLD.id OR NEW.owner_session_id!=OLD.owner_session_id OR NEW.peer_id!=OLD.peer_id
  OR NEW.remote_session_id!=OLD.remote_session_id OR NEW.payload!=OLD.payload
  OR NEW.idempotency_digest!=OLD.idempotency_digest OR NEW.request_fingerprint!=OLD.request_fingerprint
  OR NEW.created_at!=OLD.created_at OR NEW.expires_at!=OLD.expires_at
BEGIN SELECT RAISE(ABORT,'satellite message identity is immutable'); END;
CREATE TABLE satellite_inbound_policy (
    kind TEXT NOT NULL CHECK(kind IN ('hub','root')),
    value TEXT NOT NULL CHECK(rsi_uuid_is_canonical(value)),
    PRIMARY KEY(kind, value)
);
CREATE TABLE satellite_inbound_deliveries (
    message_id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(message_id)),
    hub_installation_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(hub_installation_id)),
    sender_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(sender_session_id)),
    target_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(target_session_id)),
    payload_digest TEXT NOT NULL CHECK(length(payload_digest)=64),
    received_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(received_at))
);
CREATE TRIGGER satellite_inbound_deliveries_no_update BEFORE UPDATE ON satellite_inbound_deliveries BEGIN SELECT RAISE(ABORT,'satellite inbound deliveries are immutable'); END;
CREATE TRIGGER satellite_inbound_deliveries_no_delete BEFORE DELETE ON satellite_inbound_deliveries BEGIN SELECT RAISE(ABORT,'satellite inbound deliveries are append only'); END;";

/// Catalog objects, for the fixture rewind and presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 10] = [
    ("table", "satellite_peer_scope"),
    ("table", "satellite_messages"),
    ("table", "satellite_inbound_policy"),
    ("table", "satellite_inbound_deliveries"),
    ("index", "satellite_messages_by_state"),
    ("trigger", "satellite_messages_no_delete"),
    ("trigger", "satellite_messages_settled_immutable"),
    ("trigger", "satellite_messages_identity_immutable"),
    ("trigger", "satellite_inbound_deliveries_no_update"),
    ("trigger", "satellite_inbound_deliveries_no_delete"),
];

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "satellite dispatch requires V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: satellite-dispatch-migration

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn parse_time(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| DaemonError::Store("invalid satellite message timestamp".into()))
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| DaemonError::Store("invalid satellite message id".into()))
}

/// Bound on the rotation hops walked to a declared seat's current tip.
const SATELLITE_LINEAGE_MAX_HOPS: usize = 64;

fn not_authorized() -> DaemonError {
    DaemonError::PolicyDenied(SATELLITE_TARGET_NOT_AUTHORIZED.into())
}

/// One queued row a dispatcher round works on.
#[derive(Debug, Clone)]
pub struct QueuedSatelliteMessage {
    pub id: Uuid,
    pub owner_session_id: Uuid,
    pub peer_id: Uuid,
    pub remote_session_id: Uuid,
    pub payload: String,
    pub(crate) expires_at: DateTime<Utc>,
}

/// Facts the dispatcher needs about a peer, read under one lock.
#[derive(Debug, Clone)]
pub struct DispatchPeerFacts {
    pub(crate) label: String,
    pub(crate) expected_installation_id: Uuid,
}

impl Store {
    /// Accept one message for a scoped remote session. Nothing is stored on
    /// refusal, and every existence-revealing refusal is identical.
    pub fn queue_satellite_message(
        &self,
        owner_session_id: Uuid,
        request: &AgentSendSatelliteMessageRequestV1,
        now: DateTime<Utc>,
    ) -> Result<AgentSendSatelliteMessageReceiptV1> {
        request
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let peer_id = request.peer_id.0.to_string();
        let remote_id = request.remote_session_id.0.to_string();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authorized: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM satellite_peers p
                JOIN satellite_peer_scope s ON s.peer_id=p.id
                WHERE p.id=?1 AND s.remote_session_id=?2
                  AND p.enabled=1 AND p.read_enabled=1 AND p.dispatch_enabled=1
                  AND p.expected_installation_id IS NOT NULL)",
            params![peer_id, remote_id],
            |row| row.get(0),
        )?;
        if !authorized {
            return Err(not_authorized());
        }
        let digest_key = digest(&[&request.idempotency_key]);
        let fingerprint = digest(&[&peer_id, &remote_id, &request.message]);
        let existing = tx
            .query_row(
                "SELECT id,request_fingerprint,state,expires_at,safe_error_class FROM satellite_messages
                 WHERE owner_session_id=?1 AND idempotency_digest=?2",
                params![owner_session_id.to_string(), digest_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((id, stored, state, expires, error_class)) = existing {
            if stored != fingerprint {
                return Err(DaemonError::InvalidParam(
                    SATELLITE_MESSAGE_KEY_CONFLICT.into(),
                ));
            }
            return Ok(AgentSendSatelliteMessageReceiptV1 {
                message_id: parse_uuid(&id)?,
                state,
                expires_at: parse_time(&expires)?,
                replayed: true,
                settled_error_class: error_class,
            });
        }
        let expires_at = match request.expires_at {
            Some(at) => {
                if at <= now || at > now + Duration::seconds(SATELLITE_MESSAGE_MAX_TTL_SECS) {
                    return Err(DaemonError::InvalidParam(
                        SATELLITE_MESSAGE_EXPIRY_INVALID.into(),
                    ));
                }
                at
            }
            None => now + Duration::seconds(SATELLITE_MESSAGE_DEFAULT_TTL_SECS),
        };
        let queued: i64 = tx.query_row(
            "SELECT count(*) FROM satellite_messages WHERE owner_session_id=?1 AND state='queued'",
            [owner_session_id.to_string()],
            |row| row.get(0),
        )?;
        if usize::try_from(queued).unwrap_or(usize::MAX) >= SATELLITE_MESSAGE_MAX_QUEUED {
            return Err(DaemonError::PolicyDenied(
                SATELLITE_MESSAGE_QUEUE_FULL.into(),
            ));
        }
        let id = Uuid::new_v4();
        let stamped = stamp(now);
        tx.execute(
            "INSERT INTO satellite_messages(id,owner_session_id,peer_id,remote_session_id,
                idempotency_digest,request_fingerprint,payload,created_at,expires_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?8)",
            params![
                id.to_string(),
                owner_session_id.to_string(),
                peer_id,
                remote_id,
                digest_key,
                fingerprint,
                request.message,
                stamped,
                stamp(expires_at),
            ],
        )?;
        tx.commit()?;
        Ok(AgentSendSatelliteMessageReceiptV1 {
            message_id: id,
            state: "queued".into(),
            expires_at,
            replayed: false,
            settled_error_class: None,
        })
    }

    /// Settle queued rows past their deadline as `expired`.
    pub fn expire_satellite_messages(&self, now: DateTime<Utc>) -> Result<usize> {
        let stamped = stamp(now);
        Ok(self.conn.execute(
            "UPDATE satellite_messages SET state='expired',settled_at=?1,updated_at=?1
             WHERE state='queued' AND expires_at<=?1",
            [stamped],
        )?)
    }

    /// Oldest queued rows first, bounded.
    pub fn queued_satellite_messages(&self, limit: usize) -> Result<Vec<QueuedSatelliteMessage>> {
        let mut statement = self.conn.prepare(
            "SELECT id,owner_session_id,peer_id,remote_session_id,payload,expires_at
             FROM satellite_messages WHERE state='queued' ORDER BY created_at,id LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, owner, peer, remote, payload, expires) = row?;
            out.push(QueuedSatelliteMessage {
                id: parse_uuid(&id)?,
                owner_session_id: parse_uuid(&owner)?,
                peer_id: parse_uuid(&peer)?,
                remote_session_id: parse_uuid(&remote)?,
                payload,
                expires_at: parse_time(&expires)?,
            });
        }
        Ok(out)
    }

    /// The peer facts a delivery may proceed on, or `None` when the peer is no
    /// longer enabled, paired, read-enabled and dispatch-enabled, or the
    /// target left its declared scope. Re-read on every attempt so an operator
    /// switching dispatch off stops delivery immediately.
    pub fn dispatch_peer_facts(
        &self,
        peer_id: Uuid,
        remote_session_id: Uuid,
    ) -> Result<Option<DispatchPeerFacts>> {
        let facts = self
            .conn
            .query_row(
                "SELECT p.label,p.expected_installation_id FROM satellite_peers p
                 JOIN satellite_peer_scope s ON s.peer_id=p.id
                 WHERE p.id=?1 AND s.remote_session_id=?2
                   AND p.enabled=1 AND p.read_enabled=1 AND p.dispatch_enabled=1
                   AND p.expected_installation_id IS NOT NULL",
                params![peer_id.to_string(), remote_session_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        facts
            .map(|(label, installation)| {
                Ok(DispatchPeerFacts {
                    label,
                    expected_installation_id: parse_uuid(&installation)?,
                })
            })
            .transpose()
    }

    /// #1112: the current rotation tip of `origin`'s `continued_from` lineage,
    /// where hub mail to a declared seat is delivered. Fails closed (`None`)
    /// for an unknown origin, a branched lineage (a session with more than one
    /// successor), a cycle, a chain of more than `SATELLITE_LINEAGE_MAX_HOPS`
    /// rotations, or a lineage node outside the origin's project. When the seat
    /// has rotated, `scope_root` (the declared scope root that authorized the
    /// origin) must be in the same project as the lineage.
    pub fn satellite_delivery_tip(&self, origin: Uuid, scope_root: Uuid) -> Result<Option<Uuid>> {
        let project_of = |id: Uuid| -> Result<Option<Option<String>>> {
            Ok(self
                .conn
                .query_row(
                    "SELECT project_id FROM sessions WHERE id=?1",
                    [id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?)
        };
        let Some(origin_project) = project_of(origin)? else {
            return Ok(None);
        };
        let mut statement = self
            .conn
            .prepare("SELECT id,project_id FROM sessions WHERE continued_from=?1 LIMIT 2")?;
        let mut tip = origin;
        let mut seen = std::collections::HashSet::from([origin]);
        for hop in 0..=SATELLITE_LINEAGE_MAX_HOPS {
            let successors = statement
                .query_map([tip.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let [(next, next_project)] = successors.as_slice() else {
                if !successors.is_empty() {
                    return Ok(None);
                }
                if tip == origin {
                    return Ok(Some(tip));
                }
                // Forwarded: the lineage must stay in the scope root's project.
                return Ok((project_of(scope_root)? == Some(origin_project)).then_some(tip));
            };
            if hop == SATELLITE_LINEAGE_MAX_HOPS || *next_project != origin_project {
                return Ok(None);
            }
            tip = parse_uuid(next)?;
            if !seen.insert(tip) {
                return Ok(None);
            }
        }
        Ok(None)
    }

    pub fn note_satellite_message_attempt(&self, id: Uuid, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "UPDATE satellite_messages SET attempt_count=attempt_count+1,updated_at=?2
             WHERE id=?1 AND state='queued'",
            params![id.to_string(), stamp(now)],
        )?;
        Ok(())
    }

    /// Settle one queued row. Returns false when it was already settled.
    pub fn settle_satellite_message(
        &self,
        id: Uuid,
        state: &str,
        error_class: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let stamped = stamp(now);
        let changed = self.conn.execute(
            "UPDATE satellite_messages SET state=?2,safe_error_class=?3,settled_at=?4,updated_at=?4
             WHERE id=?1 AND state='queued'",
            params![id.to_string(), state, error_class, stamped],
        )?;
        Ok(changed == 1)
    }

    pub fn satellite_message_state(&self, id: Uuid) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state,attempt_count FROM satellite_messages WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// The declared dispatch scope of one peer, sorted for a stable view.
    pub fn satellite_peer_scope(&self, peer_id: Uuid) -> Result<Vec<SatelliteUuidV1>> {
        let mut statement = self.conn.prepare(
            "SELECT remote_session_id FROM satellite_peer_scope WHERE peer_id=?1
             ORDER BY remote_session_id",
        )?;
        let rows = statement.query_map([peer_id.to_string()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(SatelliteUuidV1(parse_uuid(&row?)?));
        }
        Ok(out)
    }

    /// Operator-only: replace one peer's scope under the registry revision
    /// fence. Scope rows are operator configuration, so the replacement is an
    /// explicit delete and insert in one transaction.
    pub fn put_satellite_peer_scope(
        &self,
        expected_revision: u64,
        peer_id: Uuid,
        remote_session_ids: &[SatelliteUuidV1],
    ) -> Result<u64> {
        let mut seen = std::collections::HashSet::new();
        if remote_session_ids.len() > SATELLITE_SCOPE_MAX_SESSIONS
            || remote_session_ids
                .iter()
                .any(|id| id.0.is_nil() || !seen.insert(id.0))
        {
            return Err(DaemonError::InvalidParam(
                "invalid satellite dispatch scope".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let next = super::satellite_registry::next_registry_revision(&tx, expected_revision)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM satellite_peers WHERE id=?1)",
            [peer_id.to_string()],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(DaemonError::InvalidParam("unknown satellite peer".into()));
        }
        let now = stamp(Utc::now());
        tx.execute(
            "DELETE FROM satellite_peer_scope WHERE peer_id=?1",
            [peer_id.to_string()],
        )?;
        for id in remote_session_ids {
            tx.execute(
                "INSERT INTO satellite_peer_scope(peer_id,remote_session_id,created_at)
                 VALUES(?1,?2,?3)",
                params![peer_id.to_string(), id.0.to_string(), now],
            )?;
        }
        tx.execute(
            "UPDATE satellite_registry SET revision=?1,updated_at=?2 WHERE singleton=1",
            params![next, now],
        )?;
        tx.commit()?;
        u64::try_from(next).map_err(|_| DaemonError::Store("invalid satellite revision".into()))
    }

    /// Satellite side: the operator-set inbound policy. Empty refuses all.
    pub fn satellite_inbound_policy(&self) -> Result<SatelliteInboundPolicyV1> {
        let mut statement = self
            .conn
            .prepare("SELECT kind,value FROM satellite_inbound_policy ORDER BY kind,value")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut policy = SatelliteInboundPolicyV1::default();
        for row in rows {
            let (kind, value) = row?;
            let id = SatelliteUuidV1(parse_uuid(&value)?);
            if kind == "hub" {
                policy.allowed_hub_installations.push(id);
            } else {
                policy.scope_roots.push(id);
            }
        }
        Ok(policy)
    }

    /// Satellite side, operator-only: replace the inbound policy atomically.
    pub fn put_satellite_inbound_policy(&self, policy: &SatelliteInboundPolicyV1) -> Result<()> {
        policy
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM satellite_inbound_policy", [])?;
        for id in &policy.allowed_hub_installations {
            tx.execute(
                "INSERT INTO satellite_inbound_policy(kind,value) VALUES('hub',?1)",
                [id.0.to_string()],
            )?;
        }
        for id in &policy.scope_roots {
            tx.execute(
                "INSERT INTO satellite_inbound_policy(kind,value) VALUES('root',?1)",
                [id.0.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The prior receipt for `message_id`, if any. A digest mismatch is a
    /// conflicting reuse of the id.
    pub fn satellite_inbound_delivery(&self, message_id: Uuid) -> Result<Option<(Uuid, String)>> {
        self.conn
            .query_row(
                "SELECT target_session_id,payload_digest FROM satellite_inbound_deliveries
                 WHERE message_id=?1",
                [message_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(target, digest)| Ok((parse_uuid(&target)?, digest)))
            .transpose()
    }

    pub fn record_satellite_inbound_delivery(
        &self,
        message_id: Uuid,
        hub_installation_id: Uuid,
        sender_session_id: Uuid,
        target_session_id: Uuid,
        payload: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO satellite_inbound_deliveries(message_id,hub_installation_id,
                sender_session_id,target_session_id,payload_digest,received_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                message_id.to_string(),
                hub_installation_id.to_string(),
                sender_session_id.to_string(),
                target_session_id.to_string(),
                inbound_payload_digest(payload),
                stamp(now),
            ],
        )?;
        Ok(())
    }
}

pub fn inbound_payload_digest(payload: &str) -> String {
    digest(&[payload])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::satellite::{SatellitePeerConfigV1, SatellitePutPeerRequestV1};
    use std::path::Path;

    struct Fixture {
        store: Store,
        owner: Uuid,
        peer: Uuid,
        remote: Uuid,
    }

    fn fixture(dispatch: bool, in_scope: bool) -> Fixture {
        let store = Store::open_in_memory().expect("in-memory store");
        let session = crate::store::tests::make_test_session();
        store.insert_session(&session).unwrap();
        let peer = Uuid::new_v4();
        let remote = Uuid::new_v4();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 1,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "work laptop".into(),
                        expected_installation_id: Some(SatelliteUuidV1(Uuid::new_v4())),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: dispatch,
                    },
                    repair_quarantine: false,
                },
                Path::new("/tmp/satellites"),
            )
            .unwrap();
        if in_scope {
            store
                .put_satellite_peer_scope(2, peer, &[SatelliteUuidV1(remote)])
                .unwrap();
        }
        Fixture {
            store,
            owner: session.id,
            peer,
            remote,
        }
    }

    fn request(fixture: &Fixture, key: &str, message: &str) -> AgentSendSatelliteMessageRequestV1 {
        AgentSendSatelliteMessageRequestV1 {
            peer_id: SatelliteUuidV1(fixture.peer),
            remote_session_id: SatelliteUuidV1(fixture.remote),
            message: message.into(),
            idempotency_key: key.into(),
            expires_at: None,
        }
    }

    fn rotation_chain(store: &Store, rotations: usize) -> (Uuid, Uuid) {
        let origin = crate::store::tests::make_test_session();
        store.insert_session(&origin).unwrap();
        let mut tip = origin.id;
        for _ in 0..rotations {
            let mut next = crate::store::tests::make_test_session();
            next.continued_from = Some(tip);
            store.insert_session(&next).unwrap();
            tip = next.id;
        }
        (origin.id, tip)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn delivery_tip_resolves_a_64_hop_chain_and_refuses_a_65_hop_chain() {
        let store = Store::open_in_memory().expect("in-memory store");
        let (origin, tip) = rotation_chain(&store, SATELLITE_LINEAGE_MAX_HOPS);
        assert_eq!(
            store.satellite_delivery_tip(origin, origin).unwrap(),
            Some(tip)
        );
        let (origin, _) = rotation_chain(&store, SATELLITE_LINEAGE_MAX_HOPS + 1);
        assert_eq!(store.satellite_delivery_tip(origin, origin).unwrap(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn delivery_tip_refuses_a_lineage_cycle_without_hanging() {
        let store = Store::open_in_memory().expect("in-memory store");
        let (origin, tip) = rotation_chain(&store, 3);
        store
            .conn
            .execute(
                "UPDATE sessions SET continued_from=?1 WHERE id=?2",
                params![tip.to_string(), origin.to_string()],
            )
            .unwrap();
        assert_eq!(store.satellite_delivery_tip(origin, origin).unwrap(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn delivery_tip_refuses_a_successor_outside_the_origin_project() {
        let store = Store::open_in_memory().expect("in-memory store");
        let (origin, tip) = rotation_chain(&store, 2);
        assert_eq!(
            store.satellite_delivery_tip(origin, origin).unwrap(),
            Some(tip)
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET project_id=NULL WHERE id=?1",
                [tip.to_string()],
            )
            .unwrap();
        assert_eq!(store.satellite_delivery_tip(origin, origin).unwrap(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn scoped_dispatch_enabled_target_queues_with_default_expiry_and_replays() {
        let f = fixture(true, true);
        let now = Utc::now();
        let first = f
            .store
            .queue_satellite_message(f.owner, &request(&f, "k1", "ping"), now)
            .unwrap();
        assert_eq!(first.state, "queued");
        assert!(!first.replayed);
        assert_eq!(
            first.expires_at,
            now + Duration::seconds(SATELLITE_MESSAGE_DEFAULT_TTL_SECS)
        );
        let replay = f
            .store
            .queue_satellite_message(f.owner, &request(&f, "k1", "ping"), now)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.message_id, first.message_id);
        let conflict = f
            .store
            .queue_satellite_message(f.owner, &request(&f, "k1", "different"), now)
            .unwrap_err();
        assert!(
            conflict
                .to_string()
                .contains(SATELLITE_MESSAGE_KEY_CONFLICT)
        );
        assert_eq!(f.store.queued_satellite_messages(10).unwrap().len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn every_existence_revealing_refusal_is_the_same_error_and_stores_nothing() {
        let unknown_peer = fixture(true, true);
        let mut req = request(&unknown_peer, "k", "x");
        req.peer_id = SatelliteUuidV1(Uuid::new_v4());
        let unknown = unknown_peer
            .store
            .queue_satellite_message(unknown_peer.owner, &req, Utc::now())
            .unwrap_err()
            .to_string();
        let dispatch_off = fixture(false, true);
        let off = dispatch_off
            .store
            .queue_satellite_message(
                dispatch_off.owner,
                &request(&dispatch_off, "k", "x"),
                Utc::now(),
            )
            .unwrap_err()
            .to_string();
        let out_of_scope = fixture(true, false);
        let scope = out_of_scope
            .store
            .queue_satellite_message(
                out_of_scope.owner,
                &request(&out_of_scope, "k", "x"),
                Utc::now(),
            )
            .unwrap_err()
            .to_string();
        assert!(unknown.contains(SATELLITE_TARGET_NOT_AUTHORIZED));
        assert_eq!(unknown, off);
        assert_eq!(unknown, scope);
        for f in [&unknown_peer, &dispatch_off, &out_of_scope] {
            assert!(f.store.queued_satellite_messages(10).unwrap().is_empty());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn switching_dispatch_off_stops_the_dispatcher_facts_immediately() {
        let f = fixture(true, true);
        assert!(
            f.store
                .dispatch_peer_facts(f.peer, f.remote)
                .unwrap()
                .is_some()
        );
        f.store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 3,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(f.peer),
                        label: "work laptop".into(),
                        expected_installation_id: f.store.satellite_registry_view().unwrap().peers
                            [0]
                        .config
                        .expected_installation_id,
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: false,
                    },
                    repair_quarantine: false,
                },
                Path::new("/tmp/satellites"),
            )
            .unwrap();
        assert!(
            f.store
                .dispatch_peer_facts(f.peer, f.remote)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn queued_rows_expire_settle_once_and_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dispatch.db");
        let (owner, peer, remote) = {
            let store = Store::open(&path).unwrap();
            let session = crate::store::tests::make_test_session();
            store.insert_session(&session).unwrap();
            let peer = Uuid::new_v4();
            let remote = Uuid::new_v4();
            store
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: 1,
                        peer: SatellitePeerConfigV1 {
                            peer_id: SatelliteUuidV1(peer),
                            label: "laptop".into(),
                            expected_installation_id: Some(SatelliteUuidV1(Uuid::new_v4())),
                            enabled: true,
                            read_enabled: true,
                            dispatch_enabled: true,
                        },
                        repair_quarantine: false,
                    },
                    Path::new("/tmp/satellites"),
                )
                .unwrap();
            store
                .put_satellite_peer_scope(2, peer, &[SatelliteUuidV1(remote)])
                .unwrap();
            let f = Fixture {
                store,
                owner: session.id,
                peer,
                remote,
            };
            let now = Utc::now();
            f.store
                .queue_satellite_message(f.owner, &request(&f, "keep", "stays"), now)
                .unwrap();
            let mut short = request(&f, "short", "goes");
            short.expires_at = Some(now + Duration::seconds(5));
            let short = f
                .store
                .queue_satellite_message(f.owner, &short, now)
                .unwrap();
            (f.owner, short.message_id, (peer, remote))
        };
        let _ = (owner, peer, remote);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.queued_satellite_messages(10).unwrap().len(), 2);
        let later = Utc::now() + Duration::seconds(60);
        assert_eq!(store.expire_satellite_messages(later).unwrap(), 1);
        let far = Utc::now() + Duration::seconds(SATELLITE_MESSAGE_DEFAULT_TTL_SECS + 60);
        assert_eq!(store.expire_satellite_messages(far).unwrap(), 1);
        assert!(store.queued_satellite_messages(10).unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn settled_rows_are_immutable_and_never_deleted() {
        let f = fixture(true, true);
        let now = Utc::now();
        let receipt = f
            .store
            .queue_satellite_message(f.owner, &request(&f, "k", "x"), now)
            .unwrap();
        assert!(
            f.store
                .settle_satellite_message(receipt.message_id, "sent", None, now)
                .unwrap()
        );
        assert!(
            !f.store
                .settle_satellite_message(receipt.message_id, "failed", Some("late"), now)
                .unwrap()
        );
        assert_eq!(
            f.store
                .satellite_message_state(receipt.message_id)
                .unwrap()
                .unwrap()
                .0,
            "sent"
        );
        assert!(
            f.store
                .conn
                .execute("DELETE FROM satellite_messages", [])
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn scope_edit_is_revision_fenced_and_bounded() {
        let f = fixture(true, false);
        assert!(f.store.put_satellite_peer_scope(1, f.peer, &[]).is_err());
        let ids: Vec<_> = (0..=SATELLITE_SCOPE_MAX_SESSIONS)
            .map(|_| SatelliteUuidV1(Uuid::new_v4()))
            .collect();
        assert!(f.store.put_satellite_peer_scope(2, f.peer, &ids).is_err());
        let next = f
            .store
            .put_satellite_peer_scope(2, f.peer, &[SatelliteUuidV1(f.remote)])
            .unwrap();
        assert_eq!(f.store.satellite_registry_view().unwrap().revision, next);
        assert_eq!(f.store.satellite_peer_scope(f.peer).unwrap().len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn inbound_policy_defaults_empty_and_delivery_ledger_replays() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            store.satellite_inbound_policy().unwrap(),
            SatelliteInboundPolicyV1::default()
        );
        let policy = SatelliteInboundPolicyV1 {
            allowed_hub_installations: vec![SatelliteUuidV1(Uuid::new_v4())],
            scope_roots: vec![SatelliteUuidV1(Uuid::new_v4())],
        };
        store.put_satellite_inbound_policy(&policy).unwrap();
        assert_eq!(store.satellite_inbound_policy().unwrap(), policy);
        let id = Uuid::new_v4();
        assert!(store.satellite_inbound_delivery(id).unwrap().is_none());
        let target = Uuid::new_v4();
        store
            .record_satellite_inbound_delivery(
                id,
                Uuid::new_v4(),
                Uuid::new_v4(),
                target,
                "hi",
                Utc::now(),
            )
            .unwrap();
        let (seen, digest) = store.satellite_inbound_delivery(id).unwrap().unwrap();
        assert_eq!(seen, target);
        assert_eq!(digest, inbound_payload_digest("hi"));
    }
}
