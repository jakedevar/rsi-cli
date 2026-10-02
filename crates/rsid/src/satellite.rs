//! Bounded, process-local snapshots for operator-only satellite reads.

pub(crate) mod delivery;
pub(crate) mod deploy_link;
pub(crate) mod dispatch;
pub(crate) mod hub;
pub(crate) mod link;
pub(crate) mod poll;
pub(crate) mod registry;

use crate::error::{DaemonError, Result};
#[allow(unused_imports)]
pub(crate) use crate::store_support::satellite::{
    ReportedHealth, record_reported_health, reported_health,
};
use chrono::{DateTime, Utc};
use rsi_common::Session;
use rsi_common::satellite::{
    SATELLITE_MAX_RESPONSE_BYTES, SATELLITE_MAX_SESSIONS, SATELLITE_PROTOCOL_MAJOR,
    SATELLITE_PROTOCOL_MINOR, SATELLITE_WIRE_VERSION_V1, SatelliteCapabilitiesV1,
    SatelliteHealthV1, SatelliteIdentityV1, SatelliteProtocolVersionV1, SatelliteReadLimitsV1,
    SatelliteSessionPageRequestV1, SatelliteSessionPageV1, SatelliteSessionSummaryV1,
    SatelliteUuidV1,
};
use std::collections::{HashSet, VecDeque};
use std::sync::{LazyLock, Once, OnceLock};
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
        health: None,
    }
}

static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);
static BINARY_DIGEST: OnceLock<Option<String>> = OnceLock::new();
static BINARY_DIGEST_STARTED: Once = Once::new();
/// Refuse to hash an implausibly large binary (a debug build) at all.
const MAX_HASHED_BINARY_BYTES: u64 = 512 * 1024 * 1024;

/// Pin the process start so uptime counts from daemon start, not first probe.
pub(crate) fn note_process_start() {
    LazyLock::force(&PROCESS_START);
}

fn hash_running_binary() -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
    if file.metadata().ok()?.len() > MAX_HASHED_BINARY_BYTES {
        return None;
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// The digest of the running binary, hashed once on a background thread so a
/// probe never waits on it; `None` until the hash finishes.
fn binary_digest() -> Option<String> {
    BINARY_DIGEST_STARTED.call_once(|| {
        std::thread::spawn(|| {
            let _ = BINARY_DIGEST.set(hash_running_binary());
        });
    });
    BINARY_DIGEST.get().cloned().flatten()
}

fn load_avg_1m_milli() -> Option<u32> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let load: f64 = text.split_whitespace().next()?.parse().ok()?;
    if !load.is_finite() || load < 0.0 {
        return None;
    }
    Some((load * 1000.0).min(f64::from(u32::MAX)) as u32)
}

fn disk_free_bytes(path: &std::path::Path) -> Option<u64> {
    let stats = nix::sys::statvfs::statvfs(path).ok()?;
    Some(u64::from(stats.blocks_available()).saturating_mul(u64::from(stats.fragment_size())))
}

/// Bounded, non-secret health report carried on the satellite identity.
pub(crate) fn health(store: &crate::store::Store) -> Result<SatelliteHealthV1> {
    let (sessions_running, sessions_waiting_approval, schema_version) =
        store.satellite_health_counts()?;
    Ok(SatelliteHealthV1 {
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        binary_sha256: binary_digest(),
        uptime_seconds: PROCESS_START.elapsed().as_secs(),
        schema_version: Some(schema_version),
        sessions_running,
        sessions_waiting_approval,
        disk_free_bytes: disk_free_bytes(&rsi_common::identity::data_dir()),
        load_avg_1m_milli: load_avg_1m_milli(),
        build_sha: Some(crate::daemon_info::BUILD_SHA.to_owned()),
        started_at: Some(crate::daemon_info::DaemonInfoService::global().started_at_rfc3339()),
        supervisor_mode: crate::daemon_info::supervisor_mode(),
        missing_provider_clis: crate::provider_cli::missing_provider_clis(),
        last_deploy: store.latest_agent_deploy()?.map(|row| {
            rsi_common::satellite::SatelliteDeployStatusV1 {
                deploy_id: SatelliteUuidV1(row.id),
                state: row.state,
                sha: row.sha,
            }
        }),
    })
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
