//! Durable but inert hub registry for satellite installations and links.
//!
//! This migration creates no peer and starts no connection. S3 will provide
//! operator RPC and TUI controls before any registry state can be enabled.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::store_support::satellite::{MAX_CACHED_BYTES, PeerSnapshot};
use crate::store_support::satellite_registry::{
    LinkDirection, MAX_LINKS_PER_PEER, RegistryLink, RegistryPeer,
};
use rsi_common::satellite::{
    SatelliteHubSessionV1, SatelliteHubSessionsPageV1, SatelliteHubSessionsRequestV1,
    SatelliteLinkConfigV1, SatelliteLinkDirectionV1, SatelliteObservationV1, SatellitePeerConfigV1,
    SatellitePeerV1, SatellitePutLinkRequestV1, SatellitePutPeerRequestV1, SatelliteRegistryV1,
    SatelliteSessionKeyV1, SatelliteSessionSummaryV1, SatelliteUuidV1,
};

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

fn parse_stored_uuid(value: &str) -> Result<Uuid> {
    let parsed = Uuid::parse_str(value)
        .map_err(|_| DaemonError::Store("invalid satellite registry UUID".into()))?;
    if parsed.is_nil() || parsed.to_string() != value {
        return Err(DaemonError::Store("invalid satellite registry UUID".into()));
    }
    Ok(parsed)
}

pub(super) fn next_registry_revision(tx: &Transaction<'_>, expected: u64) -> Result<i64> {
    let current: i64 = tx.query_row(
        "SELECT revision FROM satellite_registry WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    if u64::try_from(current).ok() != Some(expected) {
        return Err(DaemonError::PolicyDenied(
            "satellite registry revision changed; refresh before editing".into(),
        ));
    }
    current
        .checked_add(1)
        .ok_or_else(|| DaemonError::Store("satellite registry revision overflow".into()))
}

impl Store {
    pub fn satellite_registry_revision(&self) -> Result<u64> {
        let revision: i64 = self.conn.query_row(
            "SELECT revision FROM satellite_registry WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(revision)
            .map_err(|_| DaemonError::Store("invalid satellite registry revision".into()))
    }

    /// Read one bounded operator view from the hub database. No peer socket is
    /// contacted while serving this view.
    pub fn satellite_registry_view(&self) -> Result<SatelliteRegistryV1> {
        let revision = self.satellite_registry_revision()?;
        let mut statement = self.conn.prepare(
            "SELECT id,label,expected_installation_id,enabled,read_enabled,row_version,dispatch_enabled
             FROM satellite_peers ORDER BY label,id LIMIT 129",
        )?;
        let raw = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, bool>(6)?,
            ))
        })?;
        let mut peers = Vec::new();
        for row in raw {
            let (id, label, expected, enabled, read_enabled, row_version, dispatch_enabled) = row?;
            let peer_id = parse_stored_uuid(&id)?;
            let row_version = u64::try_from(row_version)
                .map_err(|_| DaemonError::Store("invalid satellite peer version".into()))?;
            peers.push(SatellitePeerV1 {
                config: SatellitePeerConfigV1 {
                    peer_id: SatelliteUuidV1(peer_id),
                    label,
                    expected_installation_id: expected
                        .as_deref()
                        .map(parse_stored_uuid)
                        .transpose()?
                        .map(SatelliteUuidV1),
                    enabled,
                    read_enabled,
                    dispatch_enabled,
                },
                row_version,
                links: self.satellite_links_for_peer(peer_id)?,
                observation: self.satellite_observation_for_peer(peer_id)?,
                dispatch_scope: self.satellite_peer_scope(peer_id)?,
            });
        }
        if peers.len() > crate::store_support::satellite::MAX_REGISTERED_PEERS {
            return Err(DaemonError::Store("satellite peer limit exceeded".into()));
        }
        Ok(SatelliteRegistryV1 { revision, peers })
    }

    /// Page a single peer's last good cache. A stale snapshot is still useful
    /// to inspect, but its state travels with every page and it never becomes a
    /// local session in the hub.
    pub fn satellite_cached_sessions_page(
        &self,
        request: &SatelliteHubSessionsRequestV1,
    ) -> Result<SatelliteHubSessionsPageV1> {
        if request.peer_id.0.is_nil()
            || request.limit == 0
            || request.limit > 100
            || request.offset > crate::store_support::satellite::MAX_CACHED_ROWS as u32
        {
            return Err(DaemonError::InvalidParam(
                "invalid satellite cache page request".into(),
            ));
        }
        let view = self.satellite_registry_view()?;
        let peer = view
            .peers
            .into_iter()
            .find(|peer| peer.config.peer_id == request.peer_id)
            .ok_or_else(|| DaemonError::InvalidParam("satellite peer does not exist".into()))?;
        let observation = peer.observation.unwrap_or(SatelliteObservationV1 {
            state: "unconfigured".into(),
            installation_id: None,
            incarnation_id: None,
            last_observed_at: None,
            last_error: None,
            cached_session_count: 0,
            stale: true,
        });
        let allowed = peer.config.enabled
            && peer.config.read_enabled
            && peer.config.expected_installation_id == observation.installation_id;
        let json: Option<String> = if allowed {
            self.conn
                .query_row(
                    "SELECT last_good_summary_json FROM satellite_observations WHERE peer_id=?1",
                    [request.peer_id.0.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten()
        } else {
            None
        };
        let rows: Vec<SatelliteSessionSummaryV1> = json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        if rows.len() > crate::store_support::satellite::MAX_CACHED_ROWS {
            return Err(DaemonError::Store(
                "satellite cache row count exceeds limit".into(),
            ));
        }
        let offset = request.offset as usize;
        if offset > rows.len() {
            return Err(DaemonError::InvalidParam(
                "satellite cache offset exceeds row count".into(),
            ));
        }
        let end = (offset + usize::from(request.limit)).min(rows.len());
        let sessions = rows[offset..end]
            .iter()
            .cloned()
            .map(|summary| SatelliteHubSessionV1 {
                key: SatelliteSessionKeyV1 {
                    peer_id: request.peer_id,
                    remote_session_id: summary.session_id,
                },
                summary,
            })
            .collect();
        Ok(SatelliteHubSessionsPageV1 {
            peer_id: request.peer_id,
            observation,
            offset: request.offset,
            sessions,
            next_offset: (end < rows.len()).then_some(end as u32),
        })
    }

    pub(crate) fn satellite_links_for_peer(
        &self,
        peer_id: Uuid,
    ) -> Result<Vec<SatelliteLinkConfigV1>> {
        let mut statement = self.conn.prepare(
            "SELECT id,direction,socket_path,ssh_target,trust_reference,enabled,priority
             FROM satellite_links WHERE peer_id=?1 ORDER BY enabled DESC,priority,id LIMIT 5",
        )?;
        let raw = statement.query_map([peer_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut links = Vec::new();
        for row in raw {
            let (id, direction, socket_path, ssh_target, trust_reference, enabled, priority) = row?;
            let direction = match direction.as_str() {
                "dial_home_reverse" => SatelliteLinkDirectionV1::DialHomeReverse,
                "direct_local_forward" => SatelliteLinkDirectionV1::DirectLocalForward,
                _ => {
                    return Err(DaemonError::Store(
                        "invalid satellite link direction".into(),
                    ));
                }
            };
            let priority = u8::try_from(priority)
                .map_err(|_| DaemonError::Store("invalid satellite link priority".into()))?;
            links.push(SatelliteLinkConfigV1 {
                link_id: SatelliteUuidV1(parse_stored_uuid(&id)?),
                direction,
                socket_path,
                ssh_target,
                trust_reference,
                enabled,
                priority,
            });
        }
        if links.len() > MAX_LINKS_PER_PEER {
            return Err(DaemonError::Store("satellite link limit exceeded".into()));
        }
        Ok(links)
    }

    fn satellite_observation_for_peer(
        &self,
        peer_id: Uuid,
    ) -> Result<Option<SatelliteObservationV1>> {
        let row = self.conn.query_row(
            "SELECT state,installation_id,incarnation_id,last_observed_at,last_good_summary_json,last_error
             FROM satellite_observations WHERE peer_id=?1",
            [peer_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        ).optional()?;
        let Some((state, installation, incarnation, observed, cached, last_error)) = row else {
            return Ok(None);
        };
        let cached_session_count = match cached {
            Some(ref json) => {
                let rows: Vec<SatelliteSessionSummaryV1> = serde_json::from_str(json)?;
                if rows.len() > crate::store_support::satellite::MAX_CACHED_ROWS {
                    return Err(DaemonError::Store(
                        "satellite cached row count exceeds limit".into(),
                    ));
                }
                u32::try_from(rows.len()).map_err(|_| {
                    DaemonError::Store("satellite cached row count exceeds limit".into())
                })?
            }
            None => 0,
        };
        Ok(Some(SatelliteObservationV1 {
            stale: state != "healthy",
            state,
            installation_id: installation
                .as_deref()
                .map(parse_stored_uuid)
                .transpose()?
                .map(SatelliteUuidV1),
            incarnation_id: incarnation
                .as_deref()
                .map(parse_stored_uuid)
                .transpose()?
                .map(SatelliteUuidV1),
            last_observed_at: observed
                .as_deref()
                .map(|value| {
                    chrono::DateTime::parse_from_rfc3339(value)
                        .map(|value| value.with_timezone(&Utc))
                        .map_err(|_| {
                            DaemonError::Store("invalid satellite observation timestamp".into())
                        })
                })
                .transpose()?,
            last_error,
            cached_session_count,
        }))
    }

    /// Upsert operator-owned peer policy with a global revision fence. Link
    /// rows are separate so editing a label never removes a transport route.
    pub fn put_satellite_peer(
        &self,
        request: &SatellitePutPeerRequestV1,
        satellite_root: &Path,
    ) -> Result<u64> {
        let peer = &request.peer;
        if request.repair_quarantine && peer.expected_installation_id.is_none() {
            return Err(DaemonError::InvalidParam(
                "repair requires an expected satellite installation ID".into(),
            ));
        }
        RegistryPeer {
            id: peer.peer_id.0,
            label: peer.label.clone(),
            expected_installation_id: peer.expected_installation_id.map(|id| id.0),
            enabled: peer.enabled,
            read_enabled: peer.read_enabled,
            launch_enabled: false,
            dispatch_enabled: peer.dispatch_enabled,
            links: Vec::new(),
        }
        .validate(satellite_root)
        .map_err(|message| DaemonError::InvalidParam(message.into()))?;
        if peer.dispatch_enabled && !peer.read_enabled {
            return Err(DaemonError::InvalidParam(
                "dispatch requires read access to the peer".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let next = next_registry_revision(&tx, request.expected_registry_revision)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM satellite_peers WHERE id=?1)",
            [peer.peer_id.0.to_string()],
            |row| row.get(0),
        )?;
        if !exists {
            let count: i64 =
                tx.query_row("SELECT count(*) FROM satellite_peers", [], |row| row.get(0))?;
            if count >= crate::store_support::satellite::MAX_REGISTERED_PEERS as i64 {
                return Err(DaemonError::PolicyDenied(
                    "satellite peer limit reached".into(),
                ));
            }
        }
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        tx.execute(
            "INSERT INTO satellite_peers(
                id,label,expected_installation_id,enabled,read_enabled,dispatch_enabled,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?7,?6,?6)
             ON CONFLICT(id) DO UPDATE SET
                label=excluded.label,
                expected_installation_id=excluded.expected_installation_id,
                enabled=excluded.enabled,
                read_enabled=excluded.read_enabled,
                dispatch_enabled=excluded.dispatch_enabled,
                row_version=satellite_peers.row_version+1,
                updated_at=excluded.updated_at",
            params![
                peer.peer_id.0.to_string(),
                peer.label,
                peer.expected_installation_id.map(|id| id.0.to_string()),
                peer.enabled,
                peer.read_enabled,
                now,
                peer.dispatch_enabled,
            ],
        )?;
        tx.execute(
            "UPDATE satellite_observations SET state='degraded',
                last_error=CASE WHEN state='insecure' THEN 'operator repair pending recheck'
                    ELSE 'registry changed' END,updated_at=?2
             WHERE peer_id=?1 AND (state!='insecure' OR ?3=1)",
            params![peer.peer_id.0.to_string(), now, request.repair_quarantine],
        )?;
        tx.execute(
            "UPDATE satellite_registry SET revision=?1,updated_at=?2 WHERE singleton=1",
            params![next, now],
        )?;
        tx.commit()?;
        u64::try_from(next).map_err(|_| DaemonError::Store("invalid satellite revision".into()))
    }

    /// Add or update one link. An existing link cannot be silently reassigned
    /// to another peer, and an omitted link is retained until disabled.
    pub fn put_satellite_link(
        &self,
        request: &SatellitePutLinkRequestV1,
        satellite_root: &Path,
    ) -> Result<u64> {
        let link = &request.link;
        let direction = match link.direction {
            SatelliteLinkDirectionV1::DialHomeReverse => LinkDirection::DialHomeReverse,
            SatelliteLinkDirectionV1::DirectLocalForward => LinkDirection::DirectLocalForward,
        };
        RegistryLink {
            id: link.link_id.0,
            direction,
            socket_path: PathBuf::from(&link.socket_path),
            ssh_target: link.ssh_target.clone(),
            trust_reference: link.trust_reference.clone(),
            enabled: link.enabled,
            priority: link.priority,
        }
        .validate(satellite_root)
        .map_err(|message| DaemonError::InvalidParam(message.into()))?;
        if request.peer_id.0.is_nil() {
            return Err(DaemonError::InvalidParam("peer ID is nil".into()));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let next = next_registry_revision(&tx, request.expected_registry_revision)?;
        let peer_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM satellite_peers WHERE id=?1)",
            [request.peer_id.0.to_string()],
            |row| row.get(0),
        )?;
        if !peer_exists {
            return Err(DaemonError::InvalidParam(
                "satellite peer does not exist".into(),
            ));
        }
        let existing_peer: Option<String> = tx
            .query_row(
                "SELECT peer_id FROM satellite_links WHERE id=?1",
                [link.link_id.0.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if existing_peer
            .as_deref()
            .is_some_and(|id| id != request.peer_id.0.to_string())
        {
            return Err(DaemonError::PolicyDenied(
                "satellite link belongs to another peer".into(),
            ));
        }
        let direction = match direction {
            LinkDirection::DialHomeReverse => "dial_home_reverse",
            LinkDirection::DirectLocalForward => "direct_local_forward",
        };
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        tx.execute(
            "INSERT INTO satellite_links(
                id,peer_id,direction,socket_path,ssh_target,trust_reference,
                enabled,priority,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)
             ON CONFLICT(id) DO UPDATE SET
                direction=excluded.direction,
                socket_path=excluded.socket_path,
                ssh_target=excluded.ssh_target,
                trust_reference=excluded.trust_reference,
                enabled=excluded.enabled,
                priority=excluded.priority,
                row_version=satellite_links.row_version+1,
                updated_at=excluded.updated_at",
            params![
                link.link_id.0.to_string(),
                request.peer_id.0.to_string(),
                direction,
                link.socket_path,
                link.ssh_target,
                link.trust_reference,
                link.enabled,
                link.priority,
                now,
            ],
        )?;
        tx.execute(
            "UPDATE satellite_observations SET state='degraded',last_error='registry changed',updated_at=?2
             WHERE peer_id=?1 AND state!='insecure'",
            params![request.peer_id.0.to_string(), now],
        )?;
        tx.execute(
            "UPDATE satellite_registry SET revision=?1,updated_at=?2 WHERE singleton=1",
            params![next, now],
        )?;
        tx.commit()?;
        u64::try_from(next).map_err(|_| DaemonError::Store("invalid satellite revision".into()))
    }

    /// A hub restart invalidates the former healthy state until the poller
    /// rechecks the forwarded socket and installation identity.
    pub fn mark_satellite_observations_stale(&self) -> Result<()> {
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        self.conn.execute(
            "UPDATE satellite_observations SET state='degraded',
                last_error='awaiting identity recheck',updated_at=?1 WHERE state='healthy'",
            [now],
        )?;
        Ok(())
    }

    /// Running and approval-waiting session counts plus the database schema
    /// version, for the health a satellite reports on its identity.
    pub fn satellite_health_counts(&self) -> Result<(u32, u32, i32)> {
        let (running, waiting): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(status='Running'),0),COALESCE(SUM(status='WaitingApproval'),0)
             FROM sessions WHERE status IN ('Running','WaitingApproval')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let schema: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok((
            u32::try_from(running).unwrap_or(u32::MAX),
            u32::try_from(waiting).unwrap_or(u32::MAX),
            schema,
        ))
    }

    /// Persist only bounded read summaries. The peer's conversation bodies are
    /// never part of this record, and only a completed snapshot is healthy.
    pub fn record_satellite_snapshot(&self, peer_id: Uuid, snapshot: &PeerSnapshot) -> Result<()> {
        if peer_id.is_nil() || snapshot.installation_id.is_nil() || snapshot.incarnation_id.is_nil()
        {
            return Err(DaemonError::Store(
                "invalid satellite observation identity".into(),
            ));
        }
        let summary = serde_json::to_string(&snapshot.sessions)?;
        if snapshot.sessions.len() > crate::store_support::satellite::MAX_CACHED_ROWS
            || summary.len() > MAX_CACHED_BYTES
        {
            return Err(DaemonError::Store(
                "satellite observation exceeds cache limit".into(),
            ));
        }
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let changed = self.conn.execute(
            "INSERT INTO satellite_observations(
                peer_id,state,installation_id,incarnation_id,last_observed_at,
                last_good_summary_json,last_error,updated_at)
             VALUES(?1,'healthy',?2,?3,?4,?5,NULL,?4)
             ON CONFLICT(peer_id) DO UPDATE SET
                state='healthy',installation_id=excluded.installation_id,
                incarnation_id=excluded.incarnation_id,
                last_observed_at=excluded.last_observed_at,
                last_good_summary_json=excluded.last_good_summary_json,
                last_error=NULL,updated_at=excluded.updated_at
             WHERE satellite_observations.state!='insecure'",
            params![
                peer_id.to_string(),
                snapshot.installation_id.to_string(),
                snapshot.incarnation_id.to_string(),
                now,
                summary
            ],
        )?;
        if changed == 0 {
            return Err(DaemonError::PolicyDenied(
                "satellite peer is quarantined until operator repair".into(),
            ));
        }
        Ok(())
    }

    /// Record a fixed failure class while retaining the last good summary.
    /// Remote error text is deliberately not written to this table.
    pub fn record_satellite_failure(&self, peer_id: Uuid, failure: SatelliteFailure) -> Result<()> {
        if peer_id.is_nil() {
            return Err(DaemonError::Store("invalid satellite peer identity".into()));
        }
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        self.conn.execute(
            "INSERT INTO satellite_observations(peer_id,state,last_error,updated_at)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(peer_id) DO UPDATE SET
                state=excluded.state,last_error=excluded.last_error,
                updated_at=excluded.updated_at
             WHERE satellite_observations.state!='insecure' OR excluded.state='insecure'",
            params![peer_id.to_string(), failure.state(), failure.reason(), now],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SatelliteFailure {
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
                    health: None,
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn operator_registry_fences_edits_and_marks_restarted_cache_stale() {
        let store = Store::open_in_memory().unwrap();
        let root = Path::new("/tmp/satellites");
        let peer_id = SatelliteUuidV1(Uuid::new_v4());
        let installation_id = SatelliteUuidV1(Uuid::new_v4());
        let mut peer = SatellitePeerConfigV1 {
            peer_id,
            label: "work laptop".into(),
            expected_installation_id: None,
            enabled: true,
            read_enabled: false,
            dispatch_enabled: false,
        };
        assert_eq!(
            store
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: 1,
                        peer: peer.clone(),
                        repair_quarantine: false,
                    },
                    root,
                )
                .unwrap(),
            2
        );
        let first_link = SatelliteLinkConfigV1 {
            link_id: SatelliteUuidV1(Uuid::new_v4()),
            direction: SatelliteLinkDirectionV1::DialHomeReverse,
            socket_path: "/tmp/satellites/work.sock".into(),
            ssh_target: None,
            trust_reference: "ssh-config:work-laptop".into(),
            enabled: true,
            priority: 0,
        };
        assert_eq!(
            store
                .put_satellite_link(
                    &SatellitePutLinkRequestV1 {
                        expected_registry_revision: 2,
                        peer_id,
                        link: first_link.clone(),
                    },
                    root,
                )
                .unwrap(),
            3
        );
        let second_link = SatelliteLinkConfigV1 {
            link_id: SatelliteUuidV1(Uuid::new_v4()),
            direction: SatelliteLinkDirectionV1::DirectLocalForward,
            socket_path: "/tmp/satellites/work-direct.sock".into(),
            ssh_target: Some("work-laptop".into()),
            trust_reference: "ssh-config:work-laptop".into(),
            enabled: true,
            priority: 1,
        };
        assert_eq!(
            store
                .put_satellite_link(
                    &SatellitePutLinkRequestV1 {
                        expected_registry_revision: 3,
                        peer_id,
                        link: second_link,
                    },
                    root,
                )
                .unwrap(),
            4
        );
        peer.expected_installation_id = Some(installation_id);
        peer.read_enabled = true;
        assert!(
            store
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: 2,
                        peer: peer.clone(),
                        repair_quarantine: false,
                    },
                    root,
                )
                .is_err()
        );
        assert_eq!(
            store
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: 4,
                        peer: peer.clone(),
                        repair_quarantine: false,
                    },
                    root,
                )
                .unwrap(),
            5
        );
        store
            .record_satellite_snapshot(
                peer_id.0,
                &PeerSnapshot {
                    health: None,
                    installation_id: installation_id.0,
                    incarnation_id: Uuid::new_v4(),
                    sessions: vec![SatelliteSessionSummaryV1 {
                        session_id: SatelliteUuidV1(Uuid::new_v4()),
                        title: Some("remote build".into()),
                        provider: rsi_common::SessionProvider::Codex,
                        status: rsi_common::SessionStatus::Running,
                        working_dir: Some("/remote/worktree".into()),
                        created_at: Utc::now(),
                        updated_at: Utc::now(),
                    }],
                },
            )
            .unwrap();
        let live = store.satellite_registry_view().unwrap();
        assert_eq!(live.revision, 5);
        assert_eq!(live.peers.len(), 1);
        assert_eq!(live.peers[0].links.len(), 2);
        assert_eq!(live.peers[0].observation.as_ref().unwrap().state, "healthy");
        let page = store
            .satellite_cached_sessions_page(&SatelliteHubSessionsRequestV1 {
                peer_id,
                offset: 0,
                limit: 10,
            })
            .unwrap();
        assert_eq!(page.sessions.len(), 1);
        assert_eq!(page.sessions[0].key.peer_id, peer_id);
        assert_eq!(
            page.sessions[0].key.remote_session_id,
            page.sessions[0].summary.session_id
        );
        store.mark_satellite_observations_stale().unwrap();
        let restarted = store.satellite_registry_view().unwrap();
        assert!(restarted.peers[0].observation.as_ref().unwrap().stale);
        assert_eq!(
            restarted.peers[0]
                .observation
                .as_ref()
                .unwrap()
                .cached_session_count,
            1
        );
        peer.enabled = false;
        peer.read_enabled = false;
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 5,
                    peer,
                    repair_quarantine: false,
                },
                root,
            )
            .unwrap();
        let disabled = store
            .satellite_cached_sessions_page(&SatelliteHubSessionsRequestV1 {
                peer_id,
                offset: 0,
                limit: 10,
            })
            .unwrap();
        assert!(disabled.sessions.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn cached_remote_sessions_page_beyond_one_hundred_rows() {
        let store = Store::open_in_memory().unwrap();
        let root = Path::new("/tmp/satellites");
        let peer_id = SatelliteUuidV1(Uuid::new_v4());
        let installation_id = SatelliteUuidV1(Uuid::new_v4());
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 1,
                    peer: SatellitePeerConfigV1 {
                        peer_id,
                        label: "paged peer".into(),
                        expected_installation_id: Some(installation_id),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: false,
                    },
                    repair_quarantine: false,
                },
                root,
            )
            .unwrap();
        let now = Utc::now();
        let sessions: Vec<SatelliteSessionSummaryV1> = (0..123)
            .map(|index| SatelliteSessionSummaryV1 {
                session_id: SatelliteUuidV1(Uuid::new_v4()),
                title: Some(format!("remote {index}")),
                provider: rsi_common::SessionProvider::Codex,
                status: rsi_common::SessionStatus::Running,
                working_dir: None,
                created_at: now,
                updated_at: now,
            })
            .collect();
        store
            .record_satellite_snapshot(
                peer_id.0,
                &PeerSnapshot {
                    health: None,
                    installation_id: installation_id.0,
                    incarnation_id: Uuid::new_v4(),
                    sessions: sessions.clone(),
                },
            )
            .unwrap();
        let first = store
            .satellite_cached_sessions_page(&SatelliteHubSessionsRequestV1 {
                peer_id,
                offset: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(first.sessions.len(), 100);
        assert_eq!(first.next_offset, Some(100));
        let second = store
            .satellite_cached_sessions_page(&SatelliteHubSessionsRequestV1 {
                peer_id,
                offset: first.next_offset.unwrap(),
                limit: 100,
            })
            .unwrap();
        assert_eq!(second.sessions.len(), 23);
        assert_eq!(second.next_offset, None);
        assert_eq!(
            first.sessions[0].key.remote_session_id,
            sessions[0].session_id
        );
        assert_eq!(
            second.sessions[0].key.remote_session_id,
            sessions[100].session_id
        );
        assert_eq!(
            second.sessions[22].key.remote_session_id,
            sessions[122].session_id
        );
        assert!(
            first
                .sessions
                .iter()
                .chain(&second.sessions)
                .all(|row| row.key.peer_id == peer_id)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn identity_quarantine_survives_edits_and_requires_explicit_repair() {
        let store = Store::open_in_memory().unwrap();
        let root = Path::new("/tmp/satellites");
        let peer_id = SatelliteUuidV1(Uuid::new_v4());
        let installation_id = SatelliteUuidV1(Uuid::new_v4());
        let mut peer = SatellitePeerConfigV1 {
            peer_id,
            label: "work laptop".into(),
            expected_installation_id: Some(installation_id),
            enabled: true,
            read_enabled: true,
            dispatch_enabled: false,
        };
        assert_eq!(
            store
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: 1,
                        peer: peer.clone(),
                        repair_quarantine: false,
                    },
                    root,
                )
                .unwrap(),
            2
        );
        let mut link = SatelliteLinkConfigV1 {
            link_id: SatelliteUuidV1(Uuid::new_v4()),
            direction: SatelliteLinkDirectionV1::DialHomeReverse,
            socket_path: "/tmp/satellites/quarantine.sock".into(),
            ssh_target: None,
            trust_reference: "ssh-config:work-laptop".into(),
            enabled: true,
            priority: 0,
        };
        let put_link = |revision, link: SatelliteLinkConfigV1| SatellitePutLinkRequestV1 {
            expected_registry_revision: revision,
            peer_id,
            link,
        };
        store
            .put_satellite_link(&put_link(2, link.clone()), root)
            .unwrap();
        store
            .record_satellite_failure(peer_id.0, SatelliteFailure::Insecure)
            .unwrap();

        peer.label = "renamed laptop".into();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 3,
                    peer: peer.clone(),
                    repair_quarantine: false,
                },
                root,
            )
            .unwrap();
        link.priority = 1;
        store.put_satellite_link(&put_link(4, link), root).unwrap();
        store
            .record_satellite_failure(peer_id.0, SatelliteFailure::Offline)
            .unwrap();
        let snapshot = PeerSnapshot {
            health: None,
            installation_id: installation_id.0,
            incarnation_id: Uuid::new_v4(),
            sessions: vec![],
        };
        assert!(
            store
                .record_satellite_snapshot(peer_id.0, &snapshot)
                .is_err()
        );
        let observation = store
            .satellite_registry_view()
            .unwrap()
            .peers
            .remove(0)
            .observation
            .unwrap();
        assert_eq!(observation.state, "insecure");
        assert!(observation.stale);

        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 5,
                    peer,
                    repair_quarantine: true,
                },
                root,
            )
            .unwrap();
        let repaired = store
            .satellite_registry_view()
            .unwrap()
            .peers
            .remove(0)
            .observation
            .unwrap();
        assert_eq!(repaired.state, "degraded");
        assert_eq!(
            repaired.last_error.as_deref(),
            Some("operator repair pending recheck")
        );
        store
            .record_satellite_snapshot(peer_id.0, &snapshot)
            .unwrap();
        let healthy = store
            .satellite_registry_view()
            .unwrap()
            .peers
            .remove(0)
            .observation
            .unwrap();
        assert_eq!(healthy.state, "healthy");
    }
}
