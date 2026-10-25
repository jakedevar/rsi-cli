//! Failed satellite seats from the hub's existing poll and report caches.
//! Reports enrich the reason; only the current appointment and a matching
//! Failed session in a paired snapshot can raise an anomaly.

use super::*;
use rsi_common::health_anomaly::KIND_SATELLITE_MANAGER_SEAT_FAILED;
use rsi_common::satellite::SatelliteSessionSummaryV1;
use rsi_common::satellite_dispatch::SATELLITE_MANAGER_SEAT_FAILED_TAG;

pub(super) fn collect(store: &Store, scope: &Scope) -> Result<Vec<HealthCondition>> {
    let mut found = Vec::new();
    for peer in store.satellite_registry_view()?.peers {
        let Some(observation) = peer.observation else {
            continue;
        };
        if !peer.config.enabled
            || !peer.config.read_enabled
            || peer.config.expected_installation_id.is_none()
            || observation.installation_id != peer.config.expected_installation_id
            || observation.state != "healthy"
        {
            continue;
        }
        let peer_id = peer.config.peer_id.0;
        // Reuse the peer's existing hub report route. Satellite project ids
        // describe remote appointments, not the local manager's wake scope.
        if !scope.is_fleet_wide()
            && !store
                .satellite_report_project(peer_id)?
                .is_some_and(|project| scope.allows(project))
        {
            continue;
        }
        let json: Option<String> = store.conn.query_row(
            "SELECT last_good_summary_json FROM satellite_observations WHERE peer_id=?1",
            [peer_id.to_string()],
            |row| row.get(0),
        )?;
        let sessions: Vec<SatelliteSessionSummaryV1> = json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_default();
        let mut details = Vec::new();
        for manager in crate::store_support::satellite::reported_manager_policies(peer_id) {
            let Some(seat) = manager.manager_session_id else {
                continue;
            };
            if !sessions.iter().any(|session| {
                session.session_id.0 == seat && session.status == SessionStatus::Failed
            }) {
                continue;
            }
            let reason = report_reason(store, peer_id, manager.project_id, seat)?;
            details.push(format!(
                "project={} seat={seat} status=Failed {reason}",
                manager.project_id,
            ));
        }
        if !details.is_empty() {
            // One condition per peer; multiple failed projects share its ignore key.
            found.push(HealthCondition::new(
                KIND_SATELLITE_MANAGER_SEAT_FAILED,
                peer_id.to_string(),
                format!("label={} {}", peer.config.label, details.join("; ")),
            ));
        }
    }
    Ok(found)
}

fn report_reason(store: &Store, peer: Uuid, project: Uuid, seat: Uuid) -> Result<String> {
    // Report notices are bounded and retained after delivery. Match the
    // authenticated peer metadata, never the report's self-declared peer id.
    let mut statement = store.conn.prepare(
        "SELECT json_extract(state_json,'$.untrusted_text')
         FROM harness_manager_notices
         WHERE kind='ledger_change' AND subject_id='satellite_report'
           AND json_extract(state_json,'$.peer_id')=?1
           AND json_extract(state_json,'$.report_kind')='RESULT'
         ORDER BY recorded_at DESC,id DESC LIMIT 128",
    )?;
    let rows = statement.query_map([peer.to_string()], |row| row.get::<_, String>(0))?;
    for row in rows {
        if let Some(reason) = parse_reason(&row?, project, seat) {
            return Ok(reason);
        }
    }
    Ok("reason=failed_status terminal_reason=unknown".into())
}

fn parse_reason(text: &str, project: Uuid, seat: Uuid) -> Option<String> {
    // Ignore the free-form limit sentence. Only bounded wire tokens enter the
    // wake detail, and a report about a predecessor cannot describe its successor.
    let mut fields = text.split(" limit=").next()?.split_whitespace();
    if fields.next()? != SATELLITE_MANAGER_SEAT_FAILED_TAG {
        return None;
    }
    let fields: HashMap<_, _> = fields.filter_map(|field| field.split_once('=')).collect();
    if fields.get("project").copied() != Some(project.to_string().as_str())
        || fields.get("seat").copied() != Some(seat.to_string().as_str())
    {
        return None;
    }
    let token = |key| {
        fields.get(key).copied().filter(|value| {
            !value.is_empty()
                && value.len() <= 96
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.:+".contains(&b))
        })
    };
    Some(format!(
        "reason={} terminal_reason={} reset_at={}",
        token("reason")?,
        token("terminal_reason")?,
        token("reset_at")?,
    ))
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-other-03")
))]
mod tests;
