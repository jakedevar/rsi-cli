//! Bounded, process-local snapshots for operator-only satellite reads.

pub(crate) mod link;
pub(crate) mod poll;
pub(crate) mod registry;

use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use rsi_common::Session;
use rsi_common::satellite::{
    SATELLITE_MAX_RESPONSE_BYTES, SATELLITE_MAX_SESSIONS, SATELLITE_PROTOCOL_MAJOR,
    SATELLITE_PROTOCOL_MINOR, SATELLITE_WIRE_VERSION_V1, SatelliteCapabilitiesV1,
    SatelliteIdentityV1, SatelliteProtocolVersionV1, SatelliteReadLimitsV1,
    SatelliteSessionPageRequestV1, SatelliteSessionPageV1, SatelliteSessionSummaryV1,
    SatelliteUuidV1,
};
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

const SNAPSHOT_LIFETIME: Duration = Duration::from_secs(120);
const MAX_SNAPSHOTS: usize = 4;

struct Snapshot {
    id: Uuid,
    created: Instant,
    observed_at: DateTime<Utc>,
    rows: Vec<SatelliteSessionSummaryV1>,
    issued_offsets: HashSet<usize>,
}

#[derive(Default)]
pub(crate) struct SatelliteSessionSnapshots {
    snapshots: Mutex<VecDeque<Snapshot>>,
}

fn display(value: &str, max_bytes: usize) -> String {
    let mut result = String::new();
    for character in value.chars().filter(|character| !character.is_control()) {
        if result.len() + character.len_utf8() > max_bytes {
            break;
        }
        result.push(character);
    }
    result
}

fn summarize(session: &Session, limits: &SatelliteReadLimitsV1) -> SatelliteSessionSummaryV1 {
    SatelliteSessionSummaryV1 {
        session_id: SatelliteUuidV1(session.id),
        title: session
            .title
            .as_deref()
            .map(|title| display(title, usize::from(limits.max_title_bytes))),
        provider: session.provider,
        status: session.status,
        working_dir: Some(display(
            &session.working_dir.to_string_lossy(),
            usize::from(limits.max_working_dir_bytes),
        )),
        created_at: session.created_at,
        updated_at: session.updated_at.max(session.created_at),
    }
}

pub(crate) fn identity(installation_id: Uuid, incarnation_id: Uuid) -> SatelliteIdentityV1 {
    SatelliteIdentityV1 {
        wire_version: SATELLITE_WIRE_VERSION_V1,
        installation_id: SatelliteUuidV1(installation_id),
        daemon_incarnation_id: SatelliteUuidV1(incarnation_id),
        protocol: SatelliteProtocolVersionV1 {
            major: SATELLITE_PROTOCOL_MAJOR,
            minor: SATELLITE_PROTOCOL_MINOR,
        },
        capabilities: SatelliteCapabilitiesV1 {
            session_read: true,
            limits: SatelliteReadLimitsV1::default(),
        },
    }
}

fn parse_cursor(cursor: &str) -> Result<(Uuid, usize)> {
    let parts: Vec<_> = cursor.split(':').collect();
    if parts.len() != 3 || parts[0] != "v1" {
        return Err(DaemonError::InvalidParam("invalid satellite cursor".into()));
    }
    let id = Uuid::parse_str(parts[1])
        .map_err(|_| DaemonError::InvalidParam("invalid satellite cursor".into()))?;
    let offset = parts[2]
        .parse::<usize>()
        .map_err(|_| DaemonError::InvalidParam("invalid satellite cursor".into()))?;
    if id.is_nil()
        || id.to_string() != parts[1]
        || offset == 0
        || offset > SATELLITE_MAX_SESSIONS as usize
        || offset.to_string() != parts[2]
    {
        return Err(DaemonError::InvalidParam("invalid satellite cursor".into()));
    }
    Ok((id, offset))
}

impl SatelliteSessionSnapshots {
    pub(crate) async fn page(
        &self,
        request: SatelliteSessionPageRequestV1,
        sessions: Option<Vec<Session>>,
        installation_id: Uuid,
        incarnation_id: Uuid,
    ) -> Result<SatelliteSessionPageV1> {
        let limits = SatelliteReadLimitsV1::default();
        request
            .validate(&limits)
            .map_err(|e| DaemonError::InvalidParam(e.to_string()))?;
        let mut snapshots = self.snapshots.lock().await;
        snapshots.retain(|snapshot| snapshot.created.elapsed() < SNAPSHOT_LIFETIME);
        let (snapshot_id, offset) = if let Some(cursor) = request.cursor.as_deref() {
            parse_cursor(cursor)?
        } else {
            let mut sessions = sessions
                .ok_or_else(|| DaemonError::Rpc("satellite snapshot unavailable".into()))?;
            if sessions.len() > SATELLITE_MAX_SESSIONS as usize {
                return Err(DaemonError::PolicyDenied(
                    "satellite session snapshot exceeds limit".into(),
                ));
            }
            sessions.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            let id = Uuid::new_v4();
            snapshots.push_back(Snapshot {
                id,
                created: Instant::now(),
                observed_at: Utc::now(),
                rows: sessions
                    .into_iter()
                    .map(|session| summarize(&session, &limits))
                    .collect(),
                issued_offsets: HashSet::new(),
            });
            while snapshots.len() > MAX_SNAPSHOTS {
                snapshots.pop_front();
            }
            (id, 0)
        };
        let snapshot = snapshots
            .iter_mut()
            .find(|snapshot| snapshot.id == snapshot_id)
            .ok_or_else(|| DaemonError::InvalidParam("satellite cursor expired".into()))?;
        if offset > snapshot.rows.len()
            || (request.cursor.is_some() && !snapshot.issued_offsets.contains(&offset))
        {
            return Err(DaemonError::InvalidParam(
                "satellite cursor is not valid for this snapshot".into(),
            ));
        }
        let end = (offset + usize::from(request.limit)).min(snapshot.rows.len());
        if end < snapshot.rows.len() {
            snapshot.issued_offsets.insert(end);
        }
        let snapshot_total_sessions = u32::try_from(snapshot.rows.len())
            .map_err(|_| DaemonError::Rpc("satellite snapshot exceeds count limit".into()))?;
        let snapshot_offset = u32::try_from(offset)
            .map_err(|_| DaemonError::Rpc("satellite snapshot offset exceeds limit".into()))?;
        let page = SatelliteSessionPageV1 {
            wire_version: SATELLITE_WIRE_VERSION_V1,
            installation_id: SatelliteUuidV1(installation_id),
            daemon_incarnation_id: SatelliteUuidV1(incarnation_id),
            snapshot_id: SatelliteUuidV1(snapshot.id),
            snapshot_total_sessions,
            snapshot_offset,
            observed_at: snapshot.observed_at,
            sessions: snapshot.rows[offset..end].to_vec(),
            next_cursor: (end < snapshot.rows.len()).then(|| format!("v1:{}:{end}", snapshot.id)),
        };
        drop(snapshots);
        page.validate(&limits)
            .map_err(|e| DaemonError::Rpc(e.to_string()))?;
        if serde_json::to_vec(&page)?.len() > SATELLITE_MAX_RESPONSE_BYTES {
            return Err(DaemonError::Rpc(
                "satellite response exceeds byte limit".into(),
            ));
        }
        Ok(page)
    }
}
