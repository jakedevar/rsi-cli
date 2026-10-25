use super::*;
use crate::store_support::satellite::{PeerSnapshot, record_reported_health};
use rsi_common::satellite::{
    SatelliteHealthV1, SatellitePeerConfigV1, SatellitePutPeerRequestV1, SatelliteUuidV1,
};
use rsi_common::satellite_manager_policy::SatelliteManagerLaunchesV1;

struct Fixture {
    store: Store,
    peer: Uuid,
    project: Uuid,
    seat: Uuid,
    snapshot: PeerSnapshot,
}

impl Fixture {
    fn new() -> Self {
        let store = Store::open_in_memory().unwrap();
        let peer = Uuid::new_v4();
        let project = Uuid::new_v4();
        let seat = Uuid::new_v4();
        let installation = Uuid::new_v4();
        let root = tempfile::tempdir().unwrap();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: store.satellite_registry_revision().unwrap(),
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "laptop".into(),
                        expected_installation_id: Some(SatelliteUuidV1(installation)),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: true,
                    },
                    repair_quarantine: false,
                },
                root.path(),
            )
            .unwrap();
        let now = Utc::now();
        let mut health: SatelliteHealthV1 = serde_json::from_value(serde_json::json!({
            "daemon_version": "1.0.0", "uptime_seconds": 1,
            "sessions_running": 0, "sessions_waiting_approval": 0,
        }))
        .unwrap();
        health.manager_policies = Some(vec![SatelliteManagerLaunchesV1 {
            project_id: project,
            manager_session_id: Some(seat),
            ..Default::default()
        }]);
        let snapshot = PeerSnapshot {
            installation_id: installation,
            incarnation_id: Uuid::new_v4(),
            sessions: vec![SatelliteSessionSummaryV1 {
                session_id: SatelliteUuidV1(seat),
                title: None,
                provider: rsi_common::SessionProvider::Claude,
                status: SessionStatus::Failed,
                working_dir: None,
                created_at: now,
                updated_at: now,
            }],
            health: Some(health),
        };
        Self {
            store,
            peer,
            project,
            seat,
            snapshot,
        }
    }

    fn poll(&self) {
        self.store
            .record_satellite_snapshot(self.peer, &self.snapshot)
            .unwrap();
        record_reported_health(self.peer, self.snapshot.health.as_ref());
    }

    fn hub_manager(&self) -> (Uuid, Uuid) {
        use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
        use rsi_common::harness_manager_v2::{
            ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2,
        };
        let project = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        self.store
            .insert_project(&rsi_common::Project {
                id: project,
                name: format!("Health fixture {project}"),
                path: None,
                description: None,
                color: rsi_common::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut session = rsid_store::test_support::test_session(owner, "/tmp/health".into());
        session.project_id = Some(project);
        session.session_kind = rsi_common::SessionKind::Standard;
        session.status = SessionStatus::Running;
        self.store.insert_session(&session).unwrap();
        let config = self
            .store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: project,
                session_id: owner,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            })
            .unwrap();
        self.store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: config.row_version,
                expected_policy_version: 0,
                idempotency_key: owner.to_string(),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        (project, owner)
    }

    fn dispatch_from(&self, owner: Uuid) {
        // The same message row used by the report-route store fixtures.
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        self.store
            .conn
            .execute(
                "INSERT INTO satellite_messages(id,owner_session_id,peer_id,remote_session_id,
               idempotency_digest,request_fingerprint,payload,created_at,expires_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?5,'health fixture',?6,?6,?6)",
                params![
                    Uuid::new_v4().to_string(),
                    owner.to_string(),
                    self.peer.to_string(),
                    self.seat.to_string(),
                    "0".repeat(64),
                    now
                ],
            )
            .unwrap();
    }

    fn report(&self, peer: Uuid, text: &str) {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        self.store.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,direction,
              recipient_session_id,kind,subject_id,subject_version,state_json,recorded_at,queued_at)
             VALUES(?1,?1,?2,?3,1,'to_manager',?3,'ledger_change','satellite_report',?1,?4,?5,?5)",
            params![Uuid::new_v4().to_string(), self.project.to_string(), self.seat.to_string(),
                serde_json::json!({"peer_id":peer,"report_kind":"RESULT","untrusted_text":text}).to_string(), now],
        ).unwrap();
    }

    fn text(&self) -> String {
        rsi_common::satellite_dispatch::SatelliteManagerSeatFailedV1 {
            project: self.project,
            peer: Some(self.snapshot.installation_id),
            seat: self.seat,
            state: "exhausted",
            terminal_reason: Some("api_error"),
            reason: "manager_seat_usage_limit_hold",
            reset_at: None,
            limit_text: Some("Weekly limit resets tomorrow"),
        }
        .report_text()
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn paired_failed_manager_fires_from_cached_poll_with_report_reason_and_ignore_key() {
    let mut fixture = Fixture::new();
    fixture.poll();
    // A different authenticated peer cannot supply this seat's reason.
    fixture.report(Uuid::new_v4(), &fixture.text());
    assert!(
        collect(&fixture.store, &Scope::portfolio(vec![])).unwrap()[0]
            .detail
            .contains("terminal_reason=unknown")
    );
    fixture.report(fixture.peer, &fixture.text());
    let scope = Scope::portfolio(vec![]);
    let facts = collect_store_facts(&fixture.store, &scope, Utc::now()).unwrap();
    let inputs = Inputs {
        store: facts,
        ..Default::default()
    };
    let found = evaluate(
        &inputs,
        &HealthThresholds::default(),
        &HealthAnomalySpec::default(),
        &mut Sustain::default(),
        Utc::now(),
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, KIND_SATELLITE_MANAGER_SEAT_FAILED);
    assert_eq!(found[0].subject, fixture.peer.to_string());
    for text in [
        fixture.project.to_string(),
        fixture.seat.to_string(),
        "terminal_reason=api_error".into(),
        "reason=manager_seat_usage_limit_hold".into(),
    ] {
        assert!(found[0].detail.contains(&text), "{text}");
    }
    let spec = HealthAnomalySpec {
        ignore: vec![found[0].key()],
        ..Default::default()
    };
    assert!(spec.is_valid());
    assert!(
        evaluate(
            &inputs,
            &HealthThresholds::default(),
            &spec,
            &mut Sustain::default(),
            Utc::now()
        )
        .is_empty()
    );
    assert!(
        collect_store_facts(&fixture.store, &Scope::none(), Utc::now())
            .unwrap()
            .satellite_failed_seats
            .is_empty()
    );
    // A recovered seat clears the anomaly even with its old report retained.
    fixture.snapshot.sessions[0].status = SessionStatus::Running;
    fixture.poll();
    assert!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn project_wake_uses_existing_peer_report_route_and_fleet_keeps_all() {
    let fixture = Fixture::new();
    fixture.poll();

    let run = |scope: &Scope| {
        let inputs = Inputs {
            store: collect_store_facts(&fixture.store, scope, Utc::now()).unwrap(),
            ..Default::default()
        };
        evaluate(
            &inputs,
            &HealthThresholds::default(),
            &HealthAnomalySpec::default(),
            &mut Sustain::default(),
            Utc::now(),
        )
    };
    // Without a hub route, a matching remote UUID alone grants no visibility.
    assert!(run(&Scope::project(fixture.project)).is_empty());
    let (project, owner) = fixture.hub_manager();
    let own_scope = Scope::live(
        &fixture.store,
        Some(owner),
        &HealthAnomalySpec {
            project: Some(project),
            ..Default::default()
        },
    )
    .unwrap();
    let fleet = run(&Scope::portfolio(vec![]));
    assert_eq!(fleet.len(), 1);
    assert_eq!(fleet[0].kind, KIND_SATELLITE_MANAGER_SEAT_FAILED);
    assert_eq!(fleet[0].subject, fixture.peer.to_string());
    assert!(
        fleet[0]
            .detail
            .contains(&format!("project={}", fixture.project))
    );
    // The single manager fallback is the same route as inbound reports.
    assert_eq!(run(&own_scope), fleet);
    let (other_project, other_owner) = fixture.hub_manager();
    let other_scope = Scope::live(
        &fixture.store,
        Some(other_owner),
        &HealthAnomalySpec {
            project: Some(other_project),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(run(&own_scope).is_empty(), "ambiguous report route");
    assert!(run(&other_scope).is_empty(), "ambiguous report route");
    fixture.dispatch_from(owner);
    assert_eq!(run(&own_scope), fleet);
    assert!(run(&other_scope).is_empty(), "unrelated hub project");
    // Changing the existing dispatcher route moves visibility on the next poll.
    fixture.dispatch_from(other_owner);
    assert_eq!(run(&other_scope), fleet);
    assert!(run(&own_scope).is_empty());
    assert!(run(&Scope::project(Uuid::new_v4())).is_empty());
    assert!(run(&Scope::none()).is_empty());
    // An area observes its own seat, not project-wide satellite managers.
    assert!(
        run(&Scope {
            area_seat: Some(Uuid::new_v4()),
            ..Scope::project(other_project)
        })
        .is_empty()
    );
    // A fleet wake still sees every failed seat, even beyond its local projects.
    assert_eq!(run(&Scope::portfolio(vec![])), fleet);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn only_current_reported_managers_qualify_including_beyond_legacy_eight_rows() {
    let mut fixture = Fixture::new();
    assert!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .is_empty(),
        "not yet polled"
    );
    fixture.poll();
    fixture
        .snapshot
        .health
        .as_mut()
        .unwrap()
        .manager_policies
        .as_mut()
        .unwrap()[0]
        .manager_session_id = None;
    fixture.poll();
    assert!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .is_empty(),
        "vacant seat"
    );
    let health = fixture.snapshot.health.as_mut().unwrap();
    health.manager_policies.as_mut().unwrap()[0].manager_session_id = Some(Uuid::new_v4());
    fixture.poll();
    assert!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .is_empty(),
        "failed predecessor is not the current seat"
    );
    let health = fixture.snapshot.health.as_mut().unwrap();
    health.manager_policies = Some(
        (0..9)
            .map(|i| SatelliteManagerLaunchesV1 {
                project_id: if i == 8 {
                    fixture.project
                } else {
                    Uuid::new_v4()
                },
                manager_session_id: Some(if i == 8 { fixture.seat } else { Uuid::new_v4() }),
                ..Default::default()
            })
            .collect(),
    );
    fixture.poll();
    assert_eq!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .len(),
        1
    );
    let health = fixture.snapshot.health.as_mut().unwrap();
    health.manager_launches = vec![health.manager_policies.as_ref().unwrap()[8].clone()];
    health.manager_policies = None;
    fixture.poll();
    assert_eq!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .len(),
        1,
        "legacy heartbeat fallback"
    );
    fixture.snapshot.health.as_mut().unwrap().manager_policies = Some(vec![]);
    fixture.poll();
    assert!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .is_empty(),
        "explicit empty appointment list wins over legacy data"
    );
    fixture.snapshot.health = None;
    fixture.poll();
    assert!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .is_empty(),
        "unknown appointment"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn disabled_unpaired_mismatched_and_stale_peers_do_not_raise_seat_anomalies() {
    let fixture = Fixture::new();
    fixture.poll();
    for change in [
        "enabled=0,read_enabled=0,launch_enabled=0,dispatch_enabled=0",
        "read_enabled=0",
        "expected_installation_id=NULL,read_enabled=0,launch_enabled=0,dispatch_enabled=0",
        "expected_installation_id='11111111-1111-4111-8111-111111111111'",
    ] {
        fixture
            .store
            .conn
            .execute(
                &format!("UPDATE satellite_peers SET {change} WHERE id=?1"),
                [fixture.peer.to_string()],
            )
            .unwrap();
        assert!(
            collect(&fixture.store, &Scope::portfolio(vec![]))
                .unwrap()
                .is_empty(),
            "{change}"
        );
        fixture.store.conn.execute("UPDATE satellite_peers SET enabled=1,read_enabled=1,dispatch_enabled=1,expected_installation_id=?1 WHERE id=?2", params![fixture.snapshot.installation_id.to_string(), fixture.peer.to_string()]).unwrap();
    }
    fixture
        .store
        .record_satellite_failure(
            fixture.peer,
            crate::store::satellite_registry::SatelliteFailure::Offline,
        )
        .unwrap();
    assert!(
        collect(&fixture.store, &Scope::portfolio(vec![]))
            .unwrap()
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn reason_parser_requires_matching_seat_and_bounded_tokens() {
    let fixture = Fixture::new();
    let text = fixture.text();
    assert!(parse_reason("", fixture.project, fixture.seat).is_none());
    assert!(parse_reason("RESULT unrelated", fixture.project, fixture.seat).is_none());
    assert!(
        parse_reason(&text, fixture.project, fixture.seat)
            .unwrap()
            .contains("terminal_reason=api_error")
    );
    assert!(parse_reason(&text, Uuid::new_v4(), fixture.seat).is_none());
    assert!(parse_reason(&text, fixture.project, Uuid::new_v4()).is_none());
    assert!(
        parse_reason(
            &text.replace("terminal_reason=api_error", "terminal_reason=/private/path"),
            fixture.project,
            fixture.seat
        )
        .is_none()
    );
    assert!(
        parse_reason(
            &text.replace(
                "terminal_reason=api_error",
                &format!("terminal_reason={}", "x".repeat(97))
            ),
            fixture.project,
            fixture.seat
        )
        .is_none()
    );
}
