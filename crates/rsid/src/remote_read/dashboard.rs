//! Operator-local `RemoteGetDashboardV2` (hub #3287): the satellites the hub
//! polls and its running agents per provider and effort, labels and counts
//! only. The bounded SQL lives in the `rsid-remote-read` leaf; this owner
//! admits the read, holds the Store under the shared Remote budget and adds
//! what each satellite last reported (process-local, read after the Store
//! lock is released). No satellite is contacted while serving it.

use super::{
    DashboardSatelliteReport, ReadError, RemoteReadCompleted, RemoteReadLimiter, Result,
    dashboard_response, dashboard_source,
};
use crate::store::Store;
use chrono::Utc;
use rsi_common::remote_manager_v2::{DashboardRequestV2, DashboardResponseV2};
use rsi_common::rpc::RpcRequest;
use std::sync::Arc;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// A tokenless JSON-RPC 2.0 call whose params are exactly `{}`.
pub(super) fn parse_dashboard(rpc: &RpcRequest) -> Result<DashboardRequestV2> {
    if rpc.session_token.is_some() || rpc.jsonrpc != "2.0" {
        return Err(ReadError::InvalidSource);
    }
    serde_json::from_value(rpc.params.clone()).map_err(|_| ReadError::InvalidSource)
}

fn reported(peer_id: Uuid) -> Option<DashboardSatelliteReport> {
    crate::satellite::reported_health(peer_id).map(|health| DashboardSatelliteReport {
        build_sha: health.build_sha().map(str::to_owned),
        running_agents: health.running_agents(),
    })
}

pub(super) fn spawn_dashboard_read(
    limiter: &RemoteReadLimiter,
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    _request: DashboardRequestV2,
) -> Result<JoinHandle<RemoteReadCompleted<DashboardResponseV2>>> {
    limiter.spawn_staged_store(
        store,
        |_| Ok(()),
        |conn, (), _| dashboard_source(conn),
        |(), source, _| Ok(dashboard_response(source, reported, Utc::now())),
    )
}

#[cfg(all(
    test,
    unix,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;
    use crate::satellite::poll::PeerSnapshot;
    use rsi_common::satellite::{
        SatelliteHealthV1, SatelliteLinkConfigV1, SatelliteLinkDirectionV1, SatellitePeerConfigV1,
        SatellitePutLinkRequestV1, SatellitePutPeerRequestV1, SatelliteUuidV1,
    };
    use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    const BUILD: &str = "a077ca3b02ae7d9de512c24d56a5803d88f703e5";

    fn limiter() -> RemoteReadLimiter {
        let generous = Duration::from_secs(30);
        RemoteReadLimiter::with_deadline(generous, generous)
    }

    fn session(
        store: &Store,
        provider: SessionProvider,
        effort: Option<&str>,
        status: SessionStatus,
    ) {
        let mut session: Session = crate::session::agent_verbs::tests::test_session(
            Uuid::new_v4(),
            PathBuf::from("/home/operator/private-worktree"),
        );
        session.session_kind = SessionKind::Standard;
        session.provider = provider;
        session.effort = effort.map(str::to_owned);
        session.status = status;
        session.title = Some("operator secret title".into());
        store.insert_session(&session).unwrap();
    }

    /// A healthy, paired satellite with a reverse link whose socket path, SSH
    /// target and trust reference the dashboard must never carry.
    fn satellite(store: &Store, root: &std::path::Path, label: &str) -> (Uuid, Uuid) {
        let installation = Uuid::new_v4();
        let peer_id = SatelliteUuidV1(Uuid::new_v4());
        let revision = store.satellite_registry_revision().unwrap();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: revision,
                    peer: SatellitePeerConfigV1 {
                        peer_id,
                        label: label.into(),
                        expected_installation_id: Some(SatelliteUuidV1(installation)),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: false,
                    },
                    repair_quarantine: false,
                },
                root,
            )
            .unwrap();
        let revision = store.satellite_registry_revision().unwrap();
        store
            .put_satellite_link(
                &SatellitePutLinkRequestV1 {
                    expected_registry_revision: revision,
                    peer_id,
                    link: SatelliteLinkConfigV1 {
                        link_id: SatelliteUuidV1(Uuid::new_v4()),
                        direction: SatelliteLinkDirectionV1::DirectLocalForward,
                        socket_path: root.join("dash.sock").to_string_lossy().into_owned(),
                        ssh_target: Some("operator@192.0.2.44".into()),
                        trust_reference: "ssh-config:dashboard-trust".into(),
                        enabled: true,
                        priority: 0,
                    },
                },
                root,
            )
            .unwrap();
        store
            .record_satellite_snapshot(
                peer_id.0,
                &PeerSnapshot {
                    installation_id: installation,
                    incarnation_id: Uuid::new_v4(),
                    sessions: vec![],
                    health: None,
                },
            )
            .unwrap();
        (peer_id.0, installation)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn dashboard_params_are_closed_and_operator_only() {
        assert!(crate::remote_read::is_operator_read_method(
            rsi_common::remote_manager_v2::METHOD_DASHBOARD
        ));
        let method = rsi_common::remote_manager_v2::METHOD_DASHBOARD;
        assert_eq!(
            parse_dashboard(&RpcRequest::new(method, json!({}))).unwrap(),
            DashboardRequestV2 {}
        );
        assert!(parse_dashboard(&RpcRequest::new(method, json!({"project_id": "x"}))).is_err());
        let mut tokened = RpcRequest::new(method, json!({}));
        tokened.session_token = Some("test-token".into());
        assert!(parse_dashboard(&tokened).is_err());
        let mut wrong_version = RpcRequest::new(method, json!({}));
        wrong_version.jsonrpc = "1.0".into();
        assert!(parse_dashboard(&wrong_version).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[tokio::test]
    async fn dashboard_reads_the_live_schema_with_reported_health_and_no_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("satellites");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open_in_memory().unwrap();
        // The daemon's normal busy timeout, which the shared Remote store
        // budget requires before it lends the connection.
        store.conn.busy_timeout(Duration::from_secs(10)).unwrap();
        let (peer, installation) = satellite(&store, &root, "mac-mini");
        let health: SatelliteHealthV1 = serde_json::from_value(json!({
            "daemon_version": "0.1.0",
            "uptime_seconds": 60,
            "sessions_running": 2,
            "sessions_waiting_approval": 1,
            "build_sha": BUILD,
        }))
        .unwrap();
        crate::satellite::record_reported_health(peer, Some(&health));
        session(
            &store,
            SessionProvider::Claude,
            Some("high"),
            SessionStatus::Running,
        );
        session(
            &store,
            SessionProvider::Claude,
            Some("high"),
            SessionStatus::WaitingApproval,
        );
        session(&store, SessionProvider::Codex, None, SessionStatus::Running);
        session(
            &store,
            SessionProvider::Codex,
            Some("xhigh"),
            SessionStatus::Completed,
        );

        let completed = spawn_dashboard_read(
            &limiter(),
            Arc::new(rsid_core::store_lock::Mutex::new(store)),
            DashboardRequestV2 {},
        )
        .unwrap()
        .await
        .unwrap();
        let mut wire = Vec::new();
        completed
            .send_json_line(&mut wire, |result| result.unwrap())
            .await
            .unwrap();
        let text = String::from_utf8(wire).unwrap();
        let response: DashboardResponseV2 = serde_json::from_str(text.trim_end()).unwrap();

        assert_eq!(response.satellites.len(), 1);
        let mini = &response.satellites[0];
        assert_eq!(mini.label, "mac-mini");
        assert!(mini.reachable);
        assert_eq!(mini.build_sha.as_deref(), Some(BUILD));
        assert_eq!(mini.running_agents, Some(3));
        assert!(mini.last_seen_at.is_some());
        let groups: Vec<_> = response
            .agents
            .iter()
            .map(|g| (g.provider.as_str(), g.effort.as_deref(), g.running))
            .collect();
        assert_eq!(groups, [("Claude", Some("high"), 2), ("Codex", None, 1)]);
        assert_eq!(response.agents_total, 3);
        assert!(!response.agents_incomplete);
        // The satellite source read is bounded by the registry's own peer
        // limit, so the label order it returns is exact.
        assert_eq!(
            rsi_common::remote_manager_v2::DASHBOARD_SATELLITE_SOURCE_ROWS,
            crate::store_support::satellite::MAX_REGISTERED_PEERS
        );
        for secret in [
            "192.0.2.44",
            "operator@",
            "dash.sock",
            "dashboard-trust",
            "private-worktree",
            "secret title",
            &peer.to_string(),
            &installation.to_string(),
        ] {
            assert!(!text.contains(secret), "dashboard leaks {secret}: {text}");
        }
        crate::satellite::record_reported_health(peer, None);
    }

    /// The `EXPLAIN QUERY PLAN` detail lines of `sql` on the migrated schema.
    fn query_plan(store: &Store, sql: &str, params: &[i64]) -> Vec<String> {
        let mut statement = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        statement
            .query_map(rusqlite::params_from_iter(params), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// VM steps one run of the agent read takes on the migrated schema.
    fn agent_read_steps(store: &Store) -> i32 {
        let limit = rsi_common::remote_manager_v2::DASHBOARD_MAX_RUNNING_AGENTS + 1;
        let mut statement = store
            .conn
            .prepare(super::super::DASHBOARD_AGENT_ROWS_SQL)
            .unwrap();
        let mut rows = statement.query([i64::try_from(limit).unwrap()]).unwrap();
        while rows.next().unwrap().is_some() {}
        drop(rows);
        statement.get_status(rusqlite::StatementStatus::VmStep)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn dashboard_reads_use_the_migrated_indexes_and_visit_only_active_sessions() {
        use rsi_common::remote_manager_v2::{
            DASHBOARD_MAX_RUNNING_AGENTS, DASHBOARD_MAX_SATELLITES, DASHBOARD_SATELLITE_SOURCE_ROWS,
        };
        let store = Store::open_in_memory().unwrap();
        let limit = |rows: usize| i64::try_from(rows).unwrap();
        let plan = query_plan(
            &store,
            super::super::DASHBOARD_AGENT_ROWS_SQL,
            &[limit(DASHBOARD_MAX_RUNNING_AGENTS + 1)],
        );
        assert_eq!(
            plan,
            ["SCAN sessions USING INDEX idx_fleet_active_sessions"],
            "{plan:?}"
        );
        // The peer scan visits the registry, which holds at most its peer
        // limit; each observation is a primary-key lookup, never a scan.
        let plan = query_plan(
            &store,
            super::super::DASHBOARD_SATELLITES_SQL,
            &[
                limit(DASHBOARD_SATELLITE_SOURCE_ROWS),
                limit(DASHBOARD_MAX_SATELLITES + 1),
            ],
        );
        assert!(
            plan.iter().any(|line| line
                .starts_with("SEARCH o USING INDEX sqlite_autoindex_satellite_observations")),
            "{plan:?}"
        );
        assert!(plan.iter().all(|line| !line.contains("SCAN o")), "{plan:?}");

        // Few running sessions under a long finished history: the history is
        // not in the partial index, so the read costs what it costs without it.
        session(
            &store,
            SessionProvider::Claude,
            Some("high"),
            SessionStatus::Running,
        );
        session(
            &store,
            SessionProvider::Codex,
            None,
            SessionStatus::WaitingApproval,
        );
        session(
            &store,
            SessionProvider::Codex,
            None,
            SessionStatus::Starting,
        );
        let without_history = agent_read_steps(&store);
        for n in 0..1_500 {
            let status = if n % 2 == 0 {
                SessionStatus::Completed
            } else {
                SessionStatus::Failed
            };
            session(&store, SessionProvider::Claude, Some("high"), status);
        }
        assert_eq!(agent_read_steps(&store), without_history);
        let source = dashboard_source(&store.conn).unwrap();
        assert_eq!(source.agents_total, 2);
        assert!(!source.agents_incomplete);
    }
}
