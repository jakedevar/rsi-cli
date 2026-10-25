//! The RSI Remote v2 dashboard read (hub #3287): the satellites the hub polls
//! and its running agents per provider and effort. Labels and counts only.
//!
//! Two bounded statements read the borrowed connection, each through a hard
//! source cap before any ordering or counting: the polled satellite peers
//! (at most `DASHBOARD_SATELLITE_SOURCE_ROWS` peer rows, then label order and
//! `LIMIT` cap + 1) and the running leaf sessions (at most
//! `DASHBOARD_MAX_RUNNING_AGENTS` + 1 rows, no `ORDER BY`, read through the
//! active-sessions partial index, so SQLite visits only active sessions and
//! stops at the cap). The agent groups and the total both come from that
//! one bounded set; past the cap the response says `agents_incomplete`. What
//! a satellite last reported about itself (its build and session count) is
//! process-local daemon state, so the daemon passes it in as a value lookup
//! after the Store lock is released; this crate never reaches for it.
//!
//! Nothing selected here is a credential, link, socket path, SSH target,
//! installation id, error text or contact detail.

use super::{ReadError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::remote_manager_v2::{
    DASHBOARD_BUILD_SHA_BYTES, DASHBOARD_LABEL_BYTES, DASHBOARD_MAX_AGENT_GROUPS,
    DASHBOARD_MAX_RUNNING_AGENTS, DASHBOARD_MAX_SATELLITES, DASHBOARD_SATELLITE_SOURCE_ROWS,
    DashboardAgentGroupV2, DashboardResponseV2, DashboardSatelliteV2,
};
use rusqlite::Connection;
use std::collections::BTreeMap;
use uuid::Uuid;

/// The polled satellites: enabled, read-enabled and paired, the same set
/// `AgentGetDaemonInfo` probes. Only the label and the hub's own observation
/// are selected. The inner read stops at `?1` peer rows (no `ORDER BY`), so
/// the label sort and the join only ever see that bounded set. The inner scan
/// visits every stored peer row, which the registry caps at its peer limit
/// (the same value as `?1`), and each observation is a primary-key lookup.
pub const DASHBOARD_SATELLITES_SQL: &str = "SELECT p.id, p.label, o.state, o.last_observed_at
     FROM (SELECT id, label FROM satellite_peers
           WHERE enabled=1 AND read_enabled=1 AND expected_installation_id IS NOT NULL
           LIMIT ?1) p
     LEFT JOIN satellite_observations o ON o.peer_id=p.id
     ORDER BY p.label, p.id LIMIT ?2";

/// A running agent is a leaf session that is `Running` or `WaitingApproval`
/// (the satellite health count's definition); containers never run. No
/// `ORDER BY`, `GROUP BY` or aggregate, so SQLite stops after `?1` rows.
///
/// The read goes through `idx_fleet_active_sessions` (migration v153, a
/// partial index whose rows are only `Starting`, `Running` and
/// `WaitingApproval` sessions): the first `IN` restates that index's exact
/// predicate, and `INDEXED BY` makes SQLite refuse to prepare the statement
/// rather than fall back to a scan of every session ever stored. So the rows
/// visited are at most the active sessions, and the read stops at `?1`.
pub const DASHBOARD_AGENT_ROWS_SQL: &str = "SELECT provider, effort FROM sessions
     INDEXED BY idx_fleet_active_sessions
     WHERE status IN ('Starting','Running','WaitingApproval') AND status <> 'Starting'
       AND session_kind NOT IN ('Group','Epic')
     LIMIT ?1";

/// One polled satellite as the hub's database knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardSatelliteRow {
    pub peer_id: Uuid,
    pub label: String,
    pub reachable: bool,
    pub last_seen_at: Option<String>,
}

/// What a satellite last reported about itself (daemon process state).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashboardSatelliteReport {
    pub build_sha: Option<String>,
    pub running_agents: u32,
}

/// The Store half of the dashboard, read under the daemon's Store budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardSource {
    pub satellites: Vec<DashboardSatelliteRow>,
    pub satellites_truncated: bool,
    pub agents: Vec<DashboardAgentGroupV2>,
    pub agents_total: u32,
    pub agents_truncated: bool,
    pub agents_incomplete: bool,
}

/// Cut `text` to at most `cap` bytes at a char boundary.
fn cut(mut text: String, cap: usize) -> String {
    if text.len() > cap {
        let mut end = cap;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn sql_limit(rows: usize) -> i64 {
    i64::try_from(rows).unwrap_or(i64::MAX)
}

/// A build id is only ever hex; anything else is not shown.
fn build_sha(value: Option<String>) -> Option<String> {
    value.filter(|sha| {
        (1..=DASHBOARD_BUILD_SHA_BYTES).contains(&sha.len())
            && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

/// Read the polled satellites and the running agent groups.
///
/// # Errors
///
/// A SQLite failure, or a stored peer id that is not a UUID.
pub fn dashboard_source(conn: &Connection) -> Result<DashboardSource> {
    let satellite_cap = DASHBOARD_MAX_SATELLITES;
    let mut statement = conn.prepare_cached(DASHBOARD_SATELLITES_SQL)?;
    let mut satellites = statement
        .query_map(
            [
                sql_limit(DASHBOARD_SATELLITE_SOURCE_ROWS),
                sql_limit(satellite_cap + 1),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )?
        .map(|row| {
            let (id, label, state, last_seen_at) = row?;
            Ok(DashboardSatelliteRow {
                peer_id: Uuid::parse_str(&id).map_err(|_| ReadError::InvalidSource)?,
                label: cut(label, DASHBOARD_LABEL_BYTES),
                reachable: state.as_deref() == Some("healthy"),
                last_seen_at,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let satellites_truncated = satellites.len() > satellite_cap;
    satellites.truncate(satellite_cap);

    let (agents, agents_total, agents_truncated, agents_incomplete) = agent_groups(conn)?;
    Ok(DashboardSource {
        satellites,
        satellites_truncated,
        agents,
        agents_total,
        agents_truncated,
        agents_incomplete,
    })
}

/// Read at most [`DASHBOARD_MAX_RUNNING_AGENTS`] + 1 running agents and derive
/// the groups and the total from that one bounded set. Returns the groups
/// (largest first, at most [`DASHBOARD_MAX_AGENT_GROUPS`]), the total, whether
/// groups were cut and whether the read reached its cap.
fn agent_groups(conn: &Connection) -> Result<(Vec<DashboardAgentGroupV2>, u32, bool, bool)> {
    let row_cap = DASHBOARD_MAX_RUNNING_AGENTS;
    let mut statement = conn.prepare_cached(DASHBOARD_AGENT_ROWS_SQL)?;
    let mut rows = statement
        .query_map([sql_limit(row_cap + 1)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let incomplete = rows.len() > row_cap;
    rows.truncate(row_cap);
    let total = count(rows.len());
    let mut grouped: BTreeMap<(String, Option<String>), u32> = BTreeMap::new();
    for (provider, effort) in rows {
        let key = (
            cut(provider, DASHBOARD_LABEL_BYTES),
            effort
                .filter(|effort| !effort.trim().is_empty())
                .map(|effort| cut(effort, DASHBOARD_LABEL_BYTES)),
        );
        *grouped.entry(key).or_default() += 1;
    }
    let mut groups: Vec<DashboardAgentGroupV2> = grouped
        .into_iter()
        .map(|((provider, effort), running)| DashboardAgentGroupV2 {
            provider,
            effort,
            running,
        })
        .collect();
    // Stable sort over the map's (provider, effort) order: largest first.
    groups.sort_by(|a, b| b.running.cmp(&a.running));
    let truncated = groups.len() > DASHBOARD_MAX_AGENT_GROUPS;
    groups.truncate(DASHBOARD_MAX_AGENT_GROUPS);
    Ok((groups, total, truncated, incomplete))
}

/// Assemble the wire response. `report` looks up what a satellite last
/// reported (`None` until the hub has received a report since it started).
#[must_use]
pub fn dashboard_response(
    source: DashboardSource,
    report: impl Fn(Uuid) -> Option<DashboardSatelliteReport>,
    observed_at: DateTime<Utc>,
) -> DashboardResponseV2 {
    let satellites = source
        .satellites
        .into_iter()
        .map(|row| {
            let reported = report(row.peer_id);
            DashboardSatelliteV2 {
                label: row.label,
                reachable: row.reachable,
                build_sha: reported
                    .as_ref()
                    .and_then(|reported| build_sha(reported.build_sha.clone())),
                last_seen_at: row.last_seen_at,
                running_agents: reported.map(|reported| reported.running_agents),
            }
        })
        .collect();
    DashboardResponseV2 {
        observed_at: observed_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        satellites,
        satellites_truncated: source.satellites_truncated,
        agents: source.agents,
        agents_total: source.agents_total,
        agents_truncated: source.agents_truncated,
        agents_incomplete: source.agents_incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// The columns the dashboard reads, plus the secret-bearing columns it
    /// must never select (links, installation ids, errors, summaries).
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE satellite_peers(id TEXT PRIMARY KEY,label TEXT NOT NULL,
               expected_installation_id TEXT,enabled INTEGER NOT NULL,read_enabled INTEGER NOT NULL);
             CREATE TABLE satellite_links(id TEXT PRIMARY KEY,peer_id TEXT NOT NULL,
               socket_path TEXT NOT NULL,ssh_target TEXT,trust_reference TEXT NOT NULL);
             CREATE TABLE satellite_observations(peer_id TEXT PRIMARY KEY,state TEXT NOT NULL,
               installation_id TEXT,last_observed_at TEXT,last_good_summary_json TEXT,last_error TEXT);
             CREATE TABLE sessions(id TEXT PRIMARY KEY,provider TEXT NOT NULL,effort TEXT,
               status TEXT NOT NULL,session_kind TEXT NOT NULL,title TEXT,working_dir TEXT,
               updated_at TEXT NOT NULL DEFAULT '');
             CREATE INDEX idx_fleet_active_sessions ON sessions(updated_at DESC, id)
               WHERE status IN ('Starting','Running','WaitingApproval');",
        )
        .unwrap();
        conn
    }

    fn peer(conn: &Connection, n: u128, label: &str, polled: bool, state: Option<&str>) -> Uuid {
        let id = Uuid::from_u128(n);
        conn.execute(
            "INSERT INTO satellite_peers VALUES(?1,?2,?3,?4,?4)",
            params![
                id.to_string(),
                label,
                Uuid::from_u128(n + 1000).to_string(),
                polled
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO satellite_links VALUES(?1,?2,'/secret/peer.sock','operator@10.0.0.7','trust-secret')",
            params![Uuid::from_u128(n + 2000).to_string(), id.to_string()],
        )
        .unwrap();
        if let Some(state) = state {
            conn.execute(
                "INSERT INTO satellite_observations VALUES(?1,?2,?3,'2026-10-19T19:00:00.000000000Z',
                   '{\"token\":\"secret\"}','dial failed to 10.0.0.7')",
                params![id.to_string(), state, Uuid::from_u128(n + 3000).to_string()],
            )
            .unwrap();
        }
        id
    }

    fn session(conn: &Connection, provider: &str, effort: Option<&str>, status: &str, kind: &str) {
        conn.execute(
            "INSERT INTO sessions(id,provider,effort,status,session_kind,title,working_dir)
             VALUES(?1,?2,?3,?4,?5,'secret title','/home/operator')",
            params![Uuid::new_v4().to_string(), provider, effort, status, kind],
        )
        .unwrap();
    }

    fn observed() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-19T19:30:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn dashboard_lists_polled_satellites_and_running_agents_by_provider_and_effort() {
        let conn = fixture();
        let mini = peer(&conn, 1, "mac-mini", true, Some("healthy"));
        peer(&conn, 2, "laptop", true, Some("offline"));
        peer(&conn, 3, "spare", false, None);
        peer(&conn, 4, "new", true, None);
        session(&conn, "Claude", Some("high"), "Running", "Standard");
        session(&conn, "Claude", Some("high"), "WaitingApproval", "Task");
        session(&conn, "Claude", None, "Running", "Standard");
        session(&conn, "Codex", Some(""), "Running", "Bug");
        session(&conn, "Codex", Some("xhigh"), "Completed", "Standard");
        session(&conn, "Claude", Some("high"), "Running", "Epic");
        let source = dashboard_source(&conn).unwrap();
        let response = dashboard_response(
            source,
            |peer| {
                (peer == mini).then(|| DashboardSatelliteReport {
                    build_sha: Some("a077ca3b02ae7d9de512c24d56a5803d88f703e5".into()),
                    running_agents: 3,
                })
            },
            observed(),
        );
        assert_eq!(response.observed_at, "2026-10-19T19:30:00.000000000Z");
        let labels: Vec<_> = response
            .satellites
            .iter()
            .map(|s| s.label.as_str())
            .collect();
        assert_eq!(labels, ["laptop", "mac-mini", "new"]);
        assert_eq!(
            response.satellites[1],
            DashboardSatelliteV2 {
                label: "mac-mini".into(),
                reachable: true,
                build_sha: Some("a077ca3b02ae7d9de512c24d56a5803d88f703e5".into()),
                last_seen_at: Some("2026-10-19T19:00:00.000000000Z".into()),
                running_agents: Some(3),
            }
        );
        assert!(!response.satellites[0].reachable);
        assert_eq!(response.satellites[0].running_agents, None);
        assert_eq!(response.satellites[2].last_seen_at, None);
        assert!(!response.satellites_truncated);
        let groups: Vec<_> = response
            .agents
            .iter()
            .map(|g| (g.provider.as_str(), g.effort.as_deref(), g.running))
            .collect();
        assert_eq!(
            groups,
            [
                ("Claude", Some("high"), 2),
                ("Claude", None, 1),
                ("Codex", None, 1)
            ]
        );
        assert_eq!(response.agents_total, 4);
        assert!(!response.agents_truncated);
        assert!(!response.agents_incomplete);
    }

    #[test]
    fn dashboard_is_bounded_by_its_caps() {
        let conn = fixture();
        for n in 0..(DASHBOARD_MAX_SATELLITES as u128 + 5) {
            peer(
                &conn,
                n * 10 + 1,
                &format!("peer-{n:03}"),
                true,
                Some("healthy"),
            );
        }
        for n in 0..(DASHBOARD_MAX_AGENT_GROUPS + 5) {
            session(
                &conn,
                "Claude",
                Some(&format!("e{n:03}")),
                "Running",
                "Standard",
            );
        }
        let long = "é".repeat(200);
        session(&conn, &long, Some(&long), "Running", "Standard");
        let response = dashboard_response(
            dashboard_source(&conn).unwrap(),
            |_| {
                Some(DashboardSatelliteReport {
                    build_sha: Some("x".repeat(500)),
                    running_agents: 1,
                })
            },
            observed(),
        );
        assert_eq!(response.satellites.len(), DASHBOARD_MAX_SATELLITES);
        assert!(response.satellites_truncated);
        assert_eq!(response.agents.len(), DASHBOARD_MAX_AGENT_GROUPS);
        assert!(response.agents_truncated);
        assert_eq!(
            response.agents_total,
            u32::try_from(DASHBOARD_MAX_AGENT_GROUPS + 6).unwrap()
        );
        assert!(!response.agents_incomplete);
        for group in &response.agents {
            assert!(group.provider.len() <= DASHBOARD_LABEL_BYTES);
            assert!(
                group
                    .effort
                    .as_ref()
                    .is_none_or(|e| e.len() <= DASHBOARD_LABEL_BYTES)
            );
        }
        // A reported build that is not a hex id is not shown.
        assert!(response.satellites.iter().all(|s| s.build_sha.is_none()));
        let bytes = serde_json::to_vec(&response).unwrap();
        assert!(
            bytes.len() < 16 * 1024,
            "dashboard body {} bytes",
            bytes.len()
        );
    }

    /// `rows` running sessions plus as many finished ones, in one statement.
    fn many_sessions(conn: &Connection, rows: usize) {
        conn.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<?1)
             INSERT INTO sessions(id,provider,effort,status,session_kind,title,working_dir)
             SELECT printf('s%06d',i),
               CASE i%2 WHEN 0 THEN 'Claude' ELSE 'Codex' END, 'high',
               'Running','Standard','secret title','/home/operator' FROM n",
            [i64::try_from(rows).unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions(id,provider,effort,status,session_kind,title,working_dir)
             SELECT 'done-'||id,provider,effort,'Completed',session_kind,
               title,working_dir FROM sessions",
            [],
        )
        .unwrap();
    }

    /// VM steps one full run of `sql` takes over `conn`.
    fn vm_steps(conn: &Connection, sql: &str, limit: Option<usize>) -> i32 {
        let mut statement = conn.prepare(sql).unwrap();
        let mut rows = match limit {
            Some(limit) => statement.query([sql_limit(limit)]).unwrap(),
            None => statement.query([]).unwrap(),
        };
        while rows.next().unwrap().is_some() {}
        drop(rows);
        statement.get_status(rusqlite::StatementStatus::VmStep)
    }

    #[test]
    fn dashboard_agent_read_stops_at_its_cap_and_says_incomplete() {
        let conn = fixture();
        let cap = DASHBOARD_MAX_RUNNING_AGENTS;
        many_sessions(&conn, cap * 8);
        let response = dashboard_response(dashboard_source(&conn).unwrap(), |_| None, observed());
        assert!(response.agents_incomplete);
        assert_eq!(response.agents_total, u32::try_from(cap).unwrap());
        let counted: u32 = response.agents.iter().map(|group| group.running).sum();
        assert_eq!(counted, response.agents_total);
        assert!(!response.agents_truncated);

        // The statement reads at most cap + 1 rows: it has no sort, grouping
        // or aggregate, and it costs a fraction of one pass over every
        // matching row.
        let upper = DASHBOARD_AGENT_ROWS_SQL.to_ascii_uppercase();
        for unbounded in ["ORDER BY", "GROUP BY", "COUNT(", "DISTINCT"] {
            assert!(
                !upper.contains(unbounded),
                "{unbounded} in {DASHBOARD_AGENT_ROWS_SQL}"
            );
        }
        let mut statement = conn.prepare(DASHBOARD_AGENT_ROWS_SQL).unwrap();
        let read = statement
            .query_map([sql_limit(cap + 1)], |_| Ok(()))
            .unwrap()
            .count();
        assert_eq!(read, cap + 1);
        drop(statement);
        let bounded = vm_steps(&conn, DASHBOARD_AGENT_ROWS_SQL, Some(cap + 1));
        let every_row = vm_steps(
            &conn,
            "SELECT provider, effort FROM sessions
             WHERE status IN ('Running','WaitingApproval') AND session_kind NOT IN ('Group','Epic')",
            None,
        );
        assert!(
            bounded * 4 < every_row,
            "bounded read took {bounded} VM steps, a full pass {every_row}"
        );
    }

    #[test]
    fn dashboard_satellite_read_is_bounded_before_it_orders() {
        let upper = DASHBOARD_SATELLITES_SQL.to_ascii_uppercase();
        let inner = &upper[upper.find("(SELECT").unwrap()..upper.find(") P").unwrap()];
        assert!(inner.ends_with("LIMIT ?1"), "{inner}");
        assert!(!inner.contains("ORDER BY"), "{inner}");
        let conn = fixture();
        for n in 0..(DASHBOARD_SATELLITE_SOURCE_ROWS as u128 + 8) {
            peer(&conn, n * 10 + 1, &format!("peer-{n:03}"), true, None);
        }
        let mut statement = conn.prepare(DASHBOARD_SATELLITES_SQL).unwrap();
        let read = statement
            .query_map(
                [
                    sql_limit(DASHBOARD_SATELLITE_SOURCE_ROWS),
                    sql_limit(10_000),
                ],
                |_| Ok(()),
            )
            .unwrap()
            .count();
        assert_eq!(read, DASHBOARD_SATELLITE_SOURCE_ROWS);
        drop(statement);
        let source = dashboard_source(&conn).unwrap();
        assert_eq!(source.satellites.len(), DASHBOARD_MAX_SATELLITES);
        assert!(source.satellites_truncated);
    }

    /// The `EXPLAIN QUERY PLAN` detail lines of `sql`.
    fn query_plan(conn: &Connection, sql: &str, params: &[i64]) -> Vec<String> {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        statement
            .query_map(rusqlite::params_from_iter(params), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// `active` running, waiting and starting sessions plus `history`
    /// finished ones.
    fn active_and_history(conn: &Connection, active: usize, history: usize) {
        for n in 0..active {
            let status = ["Running", "WaitingApproval", "Starting"][n % 3];
            session(conn, "Claude", Some("high"), status, "Standard");
        }
        conn.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<?1)
             INSERT INTO sessions(id,provider,effort,status,session_kind,title,working_dir)
             SELECT printf('h%06d',i),'Codex','high',
               CASE i%2 WHEN 0 THEN 'Completed' ELSE 'Failed' END,'Standard','t','/w' FROM n",
            [i64::try_from(history).unwrap()],
        )
        .unwrap();
    }

    #[test]
    fn dashboard_agent_read_visits_only_active_sessions() {
        let limit = sql_limit(DASHBOARD_MAX_RUNNING_AGENTS + 1);
        let conn = fixture();
        let plan = query_plan(&conn, DASHBOARD_AGENT_ROWS_SQL, &[limit]);
        assert_eq!(
            plan,
            ["SCAN sessions USING INDEX idx_fleet_active_sessions"],
            "{plan:?}"
        );

        // A long history of finished sessions is not in the partial index,
        // so the read costs exactly what it costs with no history at all.
        let quiet = fixture();
        active_and_history(&quiet, 9, 0);
        let busy = fixture();
        active_and_history(&busy, 9, 20_000);
        let without_history = vm_steps(
            &quiet,
            DASHBOARD_AGENT_ROWS_SQL,
            Some(DASHBOARD_MAX_RUNNING_AGENTS + 1),
        );
        let with_history = vm_steps(
            &busy,
            DASHBOARD_AGENT_ROWS_SQL,
            Some(DASHBOARD_MAX_RUNNING_AGENTS + 1),
        );
        assert_eq!(with_history, without_history);
        let response = dashboard_response(dashboard_source(&busy).unwrap(), |_| None, observed());
        assert_eq!(response.agents_total, 6);
        assert!(!response.agents_incomplete);
        let every_row = vm_steps(
            &busy,
            "SELECT provider, effort FROM sessions
             WHERE status IN ('Running','WaitingApproval') AND session_kind NOT IN ('Group','Epic')",
            None,
        );
        assert!(
            with_history * 100 < every_row,
            "indexed read took {with_history} VM steps, a scan of every session {every_row}"
        );
    }

    #[test]
    fn dashboard_satellite_read_looks_up_each_observation_by_key() {
        let conn = fixture();
        let plan = query_plan(
            &conn,
            DASHBOARD_SATELLITES_SQL,
            &[
                sql_limit(DASHBOARD_SATELLITE_SOURCE_ROWS),
                sql_limit(DASHBOARD_MAX_SATELLITES + 1),
            ],
        );
        assert!(
            plan.iter().any(|line| line
                .starts_with("SEARCH o USING INDEX sqlite_autoindex_satellite_observations")),
            "{plan:?}"
        );
        assert!(plan.iter().all(|line| !line.contains("SCAN o")), "{plan:?}");
    }

    #[test]
    fn dashboard_never_carries_link_installation_error_or_session_detail() {
        let conn = fixture();
        let id = peer(&conn, 7, "mac-mini", true, Some("degraded"));
        session(&conn, "Claude", Some("high"), "Running", "Standard");
        let response = dashboard_response(
            dashboard_source(&conn).unwrap(),
            |_| Some(DashboardSatelliteReport::default()),
            observed(),
        );
        let text = serde_json::to_string(&response).unwrap();
        for secret in [
            "secret",
            "10.0.0.7",
            "operator@",
            "peer.sock",
            "/home/operator",
            &id.to_string(),
            &Uuid::from_u128(1007).to_string(),
            &Uuid::from_u128(3007).to_string(),
        ] {
            assert!(!text.contains(secret), "dashboard leaks {secret}: {text}");
        }
    }
}
