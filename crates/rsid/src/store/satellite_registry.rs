//! Durable but inert hub registry for satellite installations and links.
//!
//! This migration creates no peer and starts no connection. S3 will provide
//! operator RPC and TUI controls before any registry state can be enabled.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use uuid::Uuid;

use crate::satellite::poll::{MAX_CACHED_BYTES, PeerSnapshot};

// RSI-RELEASED-MIGRATION-BEGIN: satellite-registry-catalog
const REGISTRY_SQL: &str = "CREATE TABLE satellite_registry (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    revision INTEGER NOT NULL CHECK(revision>0),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
)";
const PEERS_SQL: &str = "CREATE TABLE satellite_peers (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    label TEXT NOT NULL CHECK(length(label)>0 AND length(CAST(label AS BLOB))<=128),
    expected_installation_id TEXT CHECK(expected_installation_id IS NULL OR rsi_uuid_is_canonical(expected_installation_id)),
    enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
    read_enabled INTEGER NOT NULL DEFAULT 0 CHECK(read_enabled IN (0,1)),
    launch_enabled INTEGER NOT NULL DEFAULT 0 CHECK(launch_enabled IN (0,1)),
    dispatch_enabled INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_enabled IN (0,1)),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version>0),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    CHECK((read_enabled=0 AND launch_enabled=0 AND dispatch_enabled=0)
        OR (enabled=1 AND expected_installation_id IS NOT NULL))
)";
const LINKS_SQL: &str = "CREATE TABLE satellite_links (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    peer_id TEXT NOT NULL REFERENCES satellite_peers(id) ON DELETE RESTRICT,
    direction TEXT NOT NULL CHECK(direction IN ('dial_home_reverse','direct_local_forward')),
    socket_path TEXT NOT NULL UNIQUE CHECK(length(socket_path)>0 AND length(CAST(socket_path AS BLOB))<=107),
    ssh_target TEXT CHECK(ssh_target IS NULL OR length(CAST(ssh_target AS BLOB))<=256),
    trust_reference TEXT NOT NULL CHECK(length(trust_reference)>0 AND length(CAST(trust_reference AS BLOB))<=256),
    enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)),
    priority INTEGER NOT NULL DEFAULT 0 CHECK(priority BETWEEN 0 AND 255),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version>0),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    CHECK(direction!='direct_local_forward' OR ssh_target IS NOT NULL)
)";
const LINK_ORDER_SQL: &str =
    "CREATE INDEX satellite_links_by_peer ON satellite_links(peer_id,enabled DESC,priority,id)";
const LINK_LIMIT_SQL: &str =
    "CREATE TRIGGER satellite_links_limit_insert BEFORE INSERT ON satellite_links
    WHEN (SELECT count(*) FROM satellite_links WHERE peer_id=NEW.peer_id)>=4
    BEGIN SELECT RAISE(ABORT,'satellite peer link limit exceeded'); END";
const OBSERVATIONS_SQL: &str = "CREATE TABLE satellite_observations (
    peer_id TEXT PRIMARY KEY REFERENCES satellite_peers(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK(state IN ('unconfigured','connecting','healthy','degraded','offline','insecure','incompatible','disabled')),
    installation_id TEXT CHECK(installation_id IS NULL OR rsi_uuid_is_canonical(installation_id)),
    incarnation_id TEXT CHECK(incarnation_id IS NULL OR rsi_uuid_is_canonical(incarnation_id)),
    last_observed_at TEXT CHECK(last_observed_at IS NULL OR rsi_rfc3339_nanos_is_canonical(last_observed_at)),
    last_good_summary_json TEXT CHECK(last_good_summary_json IS NULL OR (length(CAST(last_good_summary_json AS BLOB))<=1048576 AND json_valid(last_good_summary_json))),
    last_error TEXT CHECK(last_error IS NULL OR length(CAST(last_error AS BLOB))<=256),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    CHECK(state!='healthy' OR (installation_id IS NOT NULL AND incarnation_id IS NOT NULL AND last_observed_at IS NOT NULL))
)";
const CATALOG: [(&str, &str, &str); 6] = [
    ("table", "satellite_registry", REGISTRY_SQL),
    ("table", "satellite_peers", PEERS_SQL),
    ("table", "satellite_links", LINKS_SQL),
    ("index", "satellite_links_by_peer", LINK_ORDER_SQL),
    ("trigger", "satellite_links_limit_insert", LINK_LIMIT_SQL),
    ("table", "satellite_observations", OBSERVATIONS_SQL),
];
// RSI-RELEASED-MIGRATION-END: satellite-registry-catalog

// RSI-RELEASED-MIGRATION-BEGIN: satellite-registry-migration
pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let predecessor: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if predecessor != version - 1 {
        return Err(DaemonError::Store(format!(
            "satellite registry migration requires V{}, found V{predecessor}",
            version - 1
        )));
    }
    for (_, _, sql) in CATALOG {
        tx.execute_batch(sql)?;
    }
    tx.execute(
        "INSERT INTO satellite_registry(singleton,revision,updated_at) VALUES(1,1,?1)",
        params![Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)],
    )?;
    tx.pragma_update(None, "user_version", version)?;
    validate_catalog(&tx)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: satellite-registry-migration

// RSI-RELEASED-MIGRATION-BEGIN: satellite-registry-validator
pub(crate) fn validate_catalog(conn: &Connection) -> Result<()> {
    for (kind, name, expected) in CATALOG {
        let actual: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |row| row.get(0),
            )
            .map_err(|_| {
                DaemonError::Store(format!("satellite registry catalog missing {name}"))
            })?;
        if actual != expected {
            return Err(DaemonError::Store(format!(
                "satellite registry catalog changed {name}"
            )));
        }
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: satellite-registry-validator

impl Store {
    pub(crate) fn satellite_registry_revision(&self) -> Result<u64> {
        let revision: i64 = self.conn.query_row(
            "SELECT revision FROM satellite_registry WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(revision)
            .map_err(|_| DaemonError::Store("invalid satellite registry revision".into()))
    }

    /// Persist only bounded read summaries. The peer's conversation bodies are
    /// never part of this record, and only a completed snapshot is healthy.
    pub(crate) fn record_satellite_snapshot(
        &self,
        peer_id: Uuid,
        snapshot: &PeerSnapshot,
    ) -> Result<()> {
        if peer_id.is_nil() || snapshot.installation_id.is_nil() || snapshot.incarnation_id.is_nil()
        {
            return Err(DaemonError::Store(
                "invalid satellite observation identity".into(),
            ));
        }
        let summary = serde_json::to_string(&snapshot.sessions)?;
        if summary.len() > MAX_CACHED_BYTES {
            return Err(DaemonError::Store(
                "satellite observation exceeds cache limit".into(),
            ));
        }
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        self.conn.execute(
            "INSERT INTO satellite_observations(
                peer_id,state,installation_id,incarnation_id,last_observed_at,
                last_good_summary_json,last_error,updated_at)
             VALUES(?1,'healthy',?2,?3,?4,?5,NULL,?4)
             ON CONFLICT(peer_id) DO UPDATE SET
                state='healthy',installation_id=excluded.installation_id,
                incarnation_id=excluded.incarnation_id,
                last_observed_at=excluded.last_observed_at,
                last_good_summary_json=excluded.last_good_summary_json,
                last_error=NULL,updated_at=excluded.updated_at",
            params![
                peer_id.to_string(),
                snapshot.installation_id.to_string(),
                snapshot.incarnation_id.to_string(),
                now,
                summary
            ],
        )?;
        Ok(())
    }

    /// Record a fixed failure class while retaining the last good summary.
    /// Remote error text is deliberately not written to this table.
    pub(crate) fn record_satellite_failure(
        &self,
        peer_id: Uuid,
        failure: SatelliteFailure,
    ) -> Result<()> {
        if peer_id.is_nil() {
            return Err(DaemonError::Store("invalid satellite peer identity".into()));
        }
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        self.conn.execute(
            "INSERT INTO satellite_observations(peer_id,state,last_error,updated_at)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(peer_id) DO UPDATE SET
                state=excluded.state,last_error=excluded.last_error,
                updated_at=excluded.updated_at",
            params![peer_id.to_string(), failure.state(), failure.reason(), now],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SatelliteFailure {
    Offline,
    Insecure,
    Incompatible,
    Degraded,
}

impl SatelliteFailure {
    const fn state(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Insecure => "insecure",
            Self::Incompatible => "incompatible",
            Self::Degraded => "degraded",
        }
    }

    const fn reason(self) -> &'static str {
        match self {
            Self::Offline => "peer unavailable",
            Self::Insecure => "socket or identity custody failed",
            Self::Incompatible => "peer read protocol incompatible",
            Self::Degraded => "bounded peer read failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn inert_registry_survives_restart_without_peers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        let first = Store::open(&path).unwrap();
        assert_eq!(first.satellite_registry_revision().unwrap(), 1);
        let peers: i64 = first
            .conn
            .query_row("SELECT count(*) FROM satellite_peers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(peers, 0);
        validate_catalog(&first.conn).unwrap();
        let peer = Uuid::new_v4();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        first.conn.execute(
            "INSERT INTO satellite_peers(id,label,created_at,updated_at) VALUES(?1,'test peer',?2,?2)",
            params![peer.to_string(), now],
        ).unwrap();
        first
            .record_satellite_snapshot(
                peer,
                &PeerSnapshot {
                    installation_id: Uuid::new_v4(),
                    incarnation_id: Uuid::new_v4(),
                    sessions: Vec::new(),
                },
            )
            .unwrap();
        first
            .record_satellite_failure(peer, SatelliteFailure::Offline)
            .unwrap();
        drop(first);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.satellite_registry_revision().unwrap(), 1);
        let (state, cached): (String, String) = reopened
            .conn
            .query_row(
                "SELECT state,last_good_summary_json FROM satellite_observations WHERE peer_id=?1",
                params![peer.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "offline");
        assert_eq!(cached, "[]");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn two_links_share_one_peer_and_constraints_reject_bad_rows() {
        let store = Store::open_in_memory().unwrap();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let peer = Uuid::new_v4().to_string();
        store.conn.execute(
            "INSERT INTO satellite_peers(id,label,created_at,updated_at) VALUES(?1,'work laptop',?2,?2)",
            params![peer, now],
        ).unwrap();
        for (direction, suffix, target) in [
            ("dial_home_reverse", "reverse", None),
            ("direct_local_forward", "direct", Some("work-laptop")),
        ] {
            store.conn.execute(
                "INSERT INTO satellite_links(id,peer_id,direction,socket_path,ssh_target,trust_reference,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,'ssh-config:work-laptop',?6,?6)",
                params![Uuid::new_v4().to_string(), peer, direction, format!("/tmp/satellites/{suffix}.sock"), target, now],
            ).unwrap();
        }
        let links: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM satellite_links WHERE peer_id=?1",
                params![peer],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(links, 2);
        assert!(
            store
                .conn
                .execute(
                    "UPDATE satellite_peers SET read_enabled=1 WHERE id=?1",
                    params![peer]
                )
                .is_err()
        );
        assert!(store.conn.execute(
            "INSERT INTO satellite_links(id,peer_id,direction,socket_path,trust_reference,created_at,updated_at)
             VALUES(?1,?2,'direct_local_forward','/tmp/satellites/bad.sock','trust',?3,?3)",
            params![Uuid::new_v4().to_string(), peer, now]
        ).is_err());
        for index in 0..2 {
            store.conn.execute(
                "INSERT INTO satellite_links(id,peer_id,direction,socket_path,trust_reference,created_at,updated_at)
                 VALUES(?1,?2,'dial_home_reverse',?3,'trust',?4,?4)",
                params![Uuid::new_v4().to_string(), peer, format!("/tmp/satellites/extra-{index}.sock"), now],
            ).unwrap();
        }
        assert!(store.conn.execute(
            "INSERT INTO satellite_links(id,peer_id,direction,socket_path,trust_reference,created_at,updated_at)
             VALUES(?1,?2,'dial_home_reverse','/tmp/satellites/fifth.sock','trust',?3,?3)",
            params![Uuid::new_v4().to_string(), peer, now]
        ).is_err());
        assert!(store.conn.execute(
            "INSERT INTO satellite_observations(peer_id,state,last_good_summary_json,updated_at)
             VALUES(?1,'offline','not json',?2)",
            params![peer, now]
        ).is_err());
    }
}
