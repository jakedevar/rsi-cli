//! #2747: every `when.health_anomaly` condition, injected, raises exactly one
//! condition naming its kind and subject; a quiet daemon raises none.

use super::*;
use rsi_common::health_anomaly::{ALL_KINDS, HealthAnomalySpec};

fn thresholds() -> HealthThresholds {
    HealthThresholds::default()
}

fn run(inputs: &Inputs) -> Vec<HealthCondition> {
    evaluate(
        inputs,
        &thresholds(),
        &HealthAnomalySpec::default(),
        &mut Sustain::default(),
        Utc::now(),
    )
}

/// `(kind, subject)` of every condition.
fn pairs(found: &[HealthCondition]) -> Vec<(&'static str, String)> {
    found.iter().map(|c| (c.kind, c.subject.clone())).collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_quiet_daemon_raises_nothing() {
    let inputs = Inputs {
        host: HostReading {
            disk_used_pct: Some(40.0),
            cpu_c: Some(45.0),
            build_cache_reclaim_unsupported: false,
        },
        live: LiveHealthInputs {
            pm_contexts: vec![PmContext {
                project_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                fill_pct: 20.0,
                threshold_pct: 60.0,
                succession_queued: false,
            }],
            satellites: vec![SatelliteReading {
                peer_id: Uuid::new_v4(),
                label: "laptop".into(),
                disk_free_bytes: Some(200 << 30),
                disk_floor_bytes: Some(30 << 30),
                provider_faults: Vec::new(),
            }],
        },
        ..Inputs::default()
    };
    assert!(run(&inputs).is_empty());
    assert!(run(&Inputs::default()).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_refused_succession_on_a_dead_seat_names_the_project() {
    let project = Uuid::new_v4();
    let mut inputs = Inputs::default();
    inputs.store.seats.push(SeatFact {
        project_id: project,
        seat_status: Some(SessionStatus::Failed),
        refusal: "manager_succession_predecessor_changed".into(),
    });
    let found = run(&inputs);
    assert_eq!(
        pairs(&found),
        vec![(KIND_PM_SEAT_BLOCKED, project.to_string())]
    );
    assert!(
        found[0]
            .detail
            .contains("manager_succession_predecessor_changed")
    );
    assert!(found[0].detail.contains("Failed"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_pm_over_its_threshold_with_no_succession_fires_after_the_grace() {
    let session = Uuid::new_v4();
    let pm = |queued: bool, fill: f64| PmContext {
        project_id: Uuid::new_v4(),
        session_id: session,
        fill_pct: fill,
        threshold_pct: 60.0,
        succession_queued: queued,
    };
    let mut inputs = Inputs::default();
    let mut tracker = Sustain::default();
    let start = Utc::now();
    let spec = HealthAnomalySpec::default();
    let check = |inputs: &Inputs, tracker: &mut Sustain, at| {
        evaluate(inputs, &thresholds(), &spec, tracker, at)
    };

    inputs.live.pm_contexts = vec![pm(false, 61.0)];
    assert!(
        check(&inputs, &mut tracker, start).is_empty(),
        "inside the grace"
    );
    let later = start + Duration::seconds(CONTEXT_FULL_GRACE_SECS + 1);
    let found = check(&inputs, &mut tracker, later);
    assert_eq!(
        pairs(&found),
        vec![(KIND_PM_CONTEXT_FULL, session.to_string())]
    );

    // A queued succession is the daemon handling it.
    inputs.live.pm_contexts = vec![pm(true, 61.0)];
    assert!(check(&inputs, &mut tracker, later).is_empty());
    // Back under the threshold restarts the clock.
    inputs.live.pm_contexts = vec![pm(false, 30.0)];
    assert!(check(&inputs, &mut tracker, later).is_empty());
    inputs.live.pm_contexts = vec![pm(false, 61.0)];
    assert!(
        check(&inputs, &mut tracker, later).is_empty(),
        "the flap restarted it"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn blocked_executions_and_overrun_attempts_fire_only_past_the_stuck_minutes() {
    let (execution, attempt) = (Uuid::new_v4(), Uuid::new_v4());
    let now = Utc::now();
    let mut inputs = Inputs::default();
    inputs.store.blocked_executions.push(BlockedExecution {
        execution_id: execution,
        blocked_since: now - Duration::minutes(29),
        reason_kind: Some("review_unsettled".into()),
    });
    inputs.store.overrun_attempts.push(OverrunAttempt {
        attempt_id: attempt,
        execution_id: execution,
        node_id: "implement".into(),
        deadline: now - Duration::minutes(29),
    });
    let spec = HealthAnomalySpec::default();
    let at =
        |inputs: &Inputs, at| evaluate(inputs, &thresholds(), &spec, &mut Sustain::default(), at);
    assert!(at(&inputs, now).is_empty(), "29 of 30 minutes");
    let found = at(&inputs, now + Duration::minutes(2));
    assert_eq!(
        pairs(&found),
        vec![
            (KIND_EXECUTION_BLOCKED, execution.to_string()),
            (KIND_ATTEMPT_PAST_WALL_LIMIT, attempt.to_string()),
        ]
    );
    // 0 turns the whole stuck family off.
    let off = HealthThresholds {
        stuck_minutes: 0,
        ..thresholds()
    };
    assert!(
        evaluate(
            &inputs,
            &off,
            &spec,
            &mut Sustain::default(),
            now + Duration::hours(5)
        )
        .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_red_qa_with_no_issue_names_the_sha_after_the_stuck_minutes() {
    let sha = "e".repeat(40);
    let now = Utc::now();
    let mut inputs = Inputs::default();
    inputs.store.red_qa = Some(RedQa {
        sha: sha.clone(),
        finished_at: now - Duration::minutes(45),
    });
    assert_eq!(pairs(&run(&inputs)), vec![(KIND_ROLLING_QA_RED, sha)]);
    inputs.store.red_qa = Some(RedQa {
        sha: "f".repeat(40),
        finished_at: Utc::now() - Duration::minutes(2),
    });
    assert!(
        run(&inputs).is_empty(),
        "a fresh red still has time to be filed"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn hub_disk_fires_at_the_threshold_and_not_below_or_when_off() {
    let host = |used| Inputs {
        host: HostReading {
            disk_used_pct: Some(used),
            cpu_c: None,
            build_cache_reclaim_unsupported: false,
        },
        ..Inputs::default()
    };
    assert!(run(&host(89.9)).is_empty());
    let found = run(&host(98.0));
    assert_eq!(pairs(&found), vec![(KIND_HUB_DISK_HIGH, "/".to_owned())]);
    assert!(found[0].detail.contains("98.0%"));
    let off = HealthThresholds {
        disk_used_pct: 0,
        ..thresholds()
    };
    assert!(
        evaluate(
            &host(99.0),
            &off,
            &HealthAnomalySpec::default(),
            &mut Sustain::default(),
            Utc::now()
        )
        .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_satellite_below_its_disk_floor_names_the_peer() {
    let peer = Uuid::new_v4();
    let satellite = |free: u64| Inputs {
        live: LiveHealthInputs {
            satellites: vec![SatelliteReading {
                peer_id: peer,
                label: "laptop".into(),
                disk_free_bytes: Some(free),
                disk_floor_bytes: Some(30 << 30),
                provider_faults: Vec::new(),
            }],
            ..LiveHealthInputs::default()
        },
        ..Inputs::default()
    };
    assert!(run(&satellite(30 << 30)).is_empty(), "at the floor is open");
    assert_eq!(
        pairs(&run(&satellite((30 << 30) - 1))),
        vec![(KIND_SATELLITE_DISK_LOW, peer.to_string())]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_hot_cpu_fires_only_once_sustained() {
    let hot = Inputs {
        host: HostReading {
            disk_used_pct: None,
            cpu_c: Some(83.0),
            build_cache_reclaim_unsupported: false,
        },
        ..Inputs::default()
    };
    let spec = HealthAnomalySpec::default();
    let mut tracker = Sustain::default();
    let start = Utc::now();
    assert!(evaluate(&hot, &thresholds(), &spec, &mut tracker, start).is_empty());
    let found = evaluate(
        &hot,
        &thresholds(),
        &spec,
        &mut tracker,
        start + Duration::seconds(THERMAL_SUSTAIN_SECS),
    );
    assert_eq!(pairs(&found), vec![(KIND_CPU_HOT, "hub".to_owned())]);
    // A cool reading in between restarts the clock.
    let cool = Inputs {
        host: HostReading {
            disk_used_pct: None,
            cpu_c: Some(60.0),
            build_cache_reclaim_unsupported: false,
        },
        ..Inputs::default()
    };
    let again = start + Duration::seconds(THERMAL_SUSTAIN_SECS + 10);
    assert!(evaluate(&cool, &thresholds(), &spec, &mut tracker, again).is_empty());
    assert!(evaluate(&hot, &thresholds(), &spec, &mut tracker, again).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn provider_faults_on_the_hub_and_a_satellite_name_host_provider_and_model() {
    let peer = Uuid::new_v4();
    let mut inputs = Inputs::default();
    inputs.store.hub_provider_faults = vec![
        ProviderFault {
            host: "hub".into(),
            provider: "Codex".into(),
            kind: SATELLITE_FAULT_MODEL_UNAVAILABLE,
            model: Some("gpt-6.1-sol".into()),
        },
        ProviderFault {
            host: "hub".into(),
            provider: "Claude".into(),
            kind: SATELLITE_FAULT_AUTH,
            model: None,
        },
    ];
    inputs.live.satellites = vec![SatelliteReading {
        peer_id: peer,
        label: "laptop".into(),
        disk_free_bytes: None,
        disk_floor_bytes: None,
        provider_faults: vec![SatelliteProviderFaultV1 {
            provider: "Codex".into(),
            kind: SATELLITE_FAULT_AUTH.into(),
            model: None,
        }],
    }];
    let found = run(&inputs);
    assert_eq!(
        pairs(&found),
        vec![
            (
                KIND_PROVIDER_MODEL_UNAVAILABLE,
                "hub:Codex:gpt-6.1-sol".to_owned()
            ),
            (KIND_PROVIDER_AUTH_FAILURE, "hub:Claude".to_owned()),
            (KIND_PROVIDER_AUTH_FAILURE, format!("{peer}:Codex")),
        ]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn ignore_keys_silence_exactly_the_accepted_condition() {
    let mut inputs = Inputs::default();
    inputs.host.disk_used_pct = Some(95.0);
    inputs.store.red_qa = Some(RedQa {
        sha: "a".repeat(40),
        finished_at: Utc::now() - Duration::hours(2),
    });
    let spec = HealthAnomalySpec {
        ignore: vec![format!("{KIND_HUB_DISK_HIGH}:/")],
        ..HealthAnomalySpec::default()
    };
    let found = evaluate(
        &inputs,
        &thresholds(),
        &spec,
        &mut Sustain::default(),
        Utc::now(),
    );
    assert_eq!(pairs(&found), vec![(KIND_ROLLING_QA_RED, "a".repeat(40))]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn the_ignore_key_a_wake_line_printed_silences_that_condition_next_time_3032() {
    let mut inputs = Inputs::default();
    inputs.host.disk_used_pct = Some(95.0);
    let now = Utc::now();
    let first = evaluate(
        &inputs,
        &thresholds(),
        &HealthAnomalySpec::default(),
        &mut Sustain::default(),
        now,
    );
    assert_eq!(pairs(&first), vec![(KIND_HUB_DISK_HIGH, "/".to_owned())]);
    // Copy the `ignore_key=` field out of the printed line, as a manager does.
    let line = first[0].line();
    let printed = line
        .split_once(" ignore_key=")
        .and_then(|(_, rest)| rest.split_once(" detail="))
        .map(|(key, _)| key.to_owned())
        .expect("the line prints an ignore_key field");
    assert_eq!(printed, format!("{KIND_HUB_DISK_HIGH}:/"));
    let spec = HealthAnomalySpec {
        ignore: vec![printed],
        ..HealthAnomalySpec::default()
    };
    assert!(spec.is_valid());
    let again = evaluate(&inputs, &thresholds(), &spec, &mut Sustain::default(), now);
    assert!(again.is_empty(), "{again:?}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn an_unsupported_build_cache_reclaim_raises_a_persistent_condition() {
    // Hub #3055 part 2: the fail-closed shim refuses every reclaim candidate, so
    // the condition holds on every evaluation, with no disk pressure needed.
    let unsupported = Inputs {
        host: HostReading {
            build_cache_reclaim_unsupported: true,
            ..HostReading::default()
        },
        ..Inputs::default()
    };
    for _ in 0..2 {
        let found = run(&unsupported);
        assert_eq!(
            pairs(&found),
            vec![(KIND_RECLAIM_UNSUPPORTED, "hub".to_owned())]
        );
        assert!(found[0].detail.contains("build_cache_reclaim=unsupported"));
    }
    let supported = Inputs {
        host: HostReading::default(),
        ..Inputs::default()
    };
    assert!(run(&supported).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn the_host_reading_reports_reclaim_unsupported_only_on_the_shim() {
    // The real reading (no test override) follows the compiled reclaim module:
    // Linux and macOS reclaim, every other platform compiles the shim.
    let reading = read_host();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        assert!(crate::sandbox::target_reclaim::RECLAIM_SUPPORTED);
        assert!(!reading.build_cache_reclaim_unsupported);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        assert!(!crate::sandbox::target_reclaim::RECLAIM_SUPPORTED);
        assert!(reading.build_cache_reclaim_unsupported);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn every_kind_is_covered_by_an_injection_test_above() {
    // The kinds this file injects, one test each. A new kind must add a test
    // and be listed here, so no condition ships unproved.
    let injected = [
        KIND_PM_SEAT_BLOCKED,
        KIND_PM_CONTEXT_FULL,
        KIND_EXECUTION_BLOCKED,
        KIND_ATTEMPT_PAST_WALL_LIMIT,
        KIND_HUB_DISK_HIGH,
        KIND_SATELLITE_DISK_LOW,
        KIND_CPU_HOT,
        KIND_PROVIDER_MODEL_UNAVAILABLE,
        KIND_PROVIDER_AUTH_FAILURE,
        KIND_ROLLING_QA_RED,
        KIND_RECLAIM_UNSUPPORTED,
    ];
    assert_eq!(injected.len(), ALL_KINDS.len());
    for kind in ALL_KINDS {
        assert!(injected.contains(&kind), "{kind}");
    }
}

fn insert_execution(
    store: &Store,
    id: Uuid,
    project: Option<Uuid>,
    status: &str,
    updated: &str,
    deadline: Option<&str>,
) {
    store
        .conn
        .execute(
            "INSERT INTO topology_executions (id, topology_name_snapshot, workflow_id,
                 definition_json, definition_digest, project_id, requested_by_kind, repo_root,
                 base_commit, custody_plan_json, status, blocked_reason_json, deadline_at,
                 created_at, updated_at)
             VALUES (?1,'t','w','{}','d',?2,'operator','/tmp',?3,'{}',?4,?5,?6,?7,?7)",
            params![
                id.to_string(),
                project.map(|p| p.to_string()),
                "b".repeat(40),
                status,
                (status == "blocked").then(|| r#"{"kind":"review_unsettled"}"#),
                deadline,
                updated,
            ],
        )
        .unwrap();
}

fn insert_attempt(store: &Store, id: Uuid, execution: Uuid, status: &str) {
    store
        .conn
        .execute(
            "INSERT INTO topology_node_attempts (id, execution_id, node_id, iteration, attempt_no,
                 node_kind, status, dedup_key, input_json, created_at, updated_at)
             VALUES (?1,?2,?1,0,1,'session',?3,?1,'{}',?4,?4)",
            params![
                id.to_string(),
                execution.to_string(),
                status,
                Utc::now().to_rfc3339()
            ],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn the_store_reads_blocked_executions_overrun_attempts_and_scopes_them_to_the_project() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let ago = |minutes: i64| (now - Duration::minutes(minutes)).to_rfc3339();
    let project = Uuid::new_v4();
    let other = Uuid::new_v4();
    let (blocked, overrun, healthy, foreign) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    insert_execution(&store, blocked, Some(project), "blocked", &ago(90), None);
    insert_execution(
        &store,
        overrun,
        Some(project),
        "running",
        &ago(5),
        Some(&ago(60)),
    );
    insert_execution(
        &store,
        healthy,
        Some(project),
        "running",
        &ago(5),
        Some(&(now + Duration::hours(1)).to_rfc3339()),
    );
    insert_execution(&store, foreign, Some(other), "blocked", &ago(90), None);
    let (running_attempt, healthy_attempt, done_attempt) =
        (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    insert_attempt(&store, running_attempt, overrun, "running");
    insert_attempt(&store, healthy_attempt, healthy, "running");
    insert_attempt(&store, done_attempt, overrun, "succeeded");

    let all = collect_store_facts(&store, &Scope::portfolio(vec![project, other]), now).unwrap();
    let mut blocked_ids: Vec<_> = all
        .blocked_executions
        .iter()
        .map(|e| e.execution_id)
        .collect();
    blocked_ids.sort();
    let mut expected = vec![blocked, foreign];
    expected.sort();
    assert_eq!(blocked_ids, expected);
    assert_eq!(
        all.blocked_executions[0].reason_kind.as_deref(),
        Some("review_unsettled")
    );
    assert_eq!(
        all.overrun_attempts
            .iter()
            .map(|a| a.attempt_id)
            .collect::<Vec<_>>(),
        vec![running_attempt],
        "only a running attempt past its execution's wall limit"
    );

    let scoped = collect_store_facts(&store, &Scope::project(project), now).unwrap();
    assert_eq!(
        scoped
            .blocked_executions
            .iter()
            .map(|e| e.execution_id)
            .collect::<Vec<_>>(),
        vec![blocked]
    );

    // And the evaluator turns them into one condition each, once stuck.
    let found = evaluate(
        &Inputs {
            store: scoped,
            ..Inputs::default()
        },
        &thresholds(),
        &HealthAnomalySpec::default(),
        &mut Sustain::default(),
        now,
    );
    assert_eq!(
        pairs(&found),
        vec![
            (KIND_EXECUTION_BLOCKED, blocked.to_string()),
            (KIND_ATTEMPT_PAST_WALL_LIMIT, running_attempt.to_string()),
        ]
    );
}

fn insert_session(
    store: &Store,
    id: Uuid,
    provider: &str,
    model: &str,
    stop: &str,
    at: DateTime<Utc>,
) {
    store
        .conn
        .execute(
            "INSERT INTO sessions (id, provider, model, query, working_dir, status, created_at,
                                   updated_at, stop_reason, session_kind, duration_ms)
             VALUES (?1,?2,?3,'q','/tmp','Failed',?4,?4,?5,'Task',2000)",
            params![id.to_string(), provider, model, at.to_rfc3339(), stop],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn the_store_reads_a_refused_model_and_an_auth_episode_as_hub_faults() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    assert!(
        collect_store_facts(&store, &Scope::none(), now)
            .unwrap()
            .hub_provider_faults
            .is_empty()
    );
    insert_session(
        &store,
        Uuid::new_v4(),
        "Codex",
        "gpt-6.1-sol",
        "provider_error:model_unavailable:gpt-6.1-sol",
        now - Duration::minutes(20),
    );
    insert_session(
        &store,
        Uuid::new_v4(),
        "Claude",
        "claude-sonnet-5-5",
        "provider_error:provider_auth_invalid",
        now - Duration::minutes(10),
    );
    let faults = collect_store_facts(&store, &Scope::none(), now)
        .unwrap()
        .hub_provider_faults;
    let kinds: Vec<_> = faults
        .iter()
        .map(|f| (f.provider.as_str(), f.kind, f.model.as_deref()))
        .collect();
    assert!(
        kinds.contains(&("Claude", SATELLITE_FAULT_AUTH, None)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&(
            "Codex",
            SATELLITE_FAULT_MODEL_UNAVAILABLE,
            Some("gpt-6.1-sol")
        )),
        "{kinds:?}"
    );
    // A satellite reports the same facts, taken from its own store.
    let on_satellite = local_provider_faults(&store, "satellite", now).unwrap();
    assert_eq!(on_satellite.len(), 2);
    assert!(on_satellite.iter().all(|f| f.host == "satellite"));
}

// ---- #2786: an attempt's real wall limit --------------------------------------

fn start_attempt(store: &Store, id: Uuid, started: DateTime<Utc>) {
    store
        .conn
        .execute(
            "UPDATE topology_node_attempts SET started_at=?2 WHERE id=?1",
            params![id.to_string(), started.to_rfc3339()],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn an_attempt_past_its_node_wall_time_is_overrun_even_with_no_execution_deadline_2786() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let project = Uuid::new_v4();
    let long_ago = (now - Duration::hours(7)).to_rfc3339();
    let (execution, stuck, young, waiting) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    // No deadline of its own: the node wall time is the only limit.
    insert_execution(&store, execution, Some(project), "running", &long_ago, None);
    for attempt in [stuck, young, waiting] {
        insert_attempt(&store, attempt, execution, "running");
    }
    start_attempt(&store, stuck, now - Duration::hours(5));
    start_attempt(&store, young, now - Duration::hours(3));
    // Started 5 h ago but spent 2 h of it waiting on an answer: 4 h of wall
    // time are not used up yet, so it is not overrun.
    start_attempt(&store, waiting, now - Duration::hours(5));
    // The executor's own event: the wait began two hours ago and is still open.
    store
        .conn
        .execute(
            "INSERT INTO topology_events (execution_id,execution_seq,node_id,attempt_id,kind,
                 actor_kind,payload_json,created_at)
             VALUES (?1,1,?2,?2,'node_wait_started','executor',?3,?4)",
            params![
                execution.to_string(),
                waiting.to_string(),
                serde_json::json!({ "detail": { "since": (now - Duration::hours(2)).to_rfc3339() } })
                    .to_string(),
                (now - Duration::hours(2)).to_rfc3339()
            ],
        )
        .unwrap();

    let facts = collect_store_facts(&store, &Scope::project(project), now).unwrap();
    assert_eq!(
        facts
            .overrun_attempts
            .iter()
            .map(|a| a.attempt_id)
            .collect::<Vec<_>>(),
        vec![stuck],
        "{:?}",
        facts.overrun_attempts
    );
    // Its limit is the node wall time from its own start (4 h => 1 h ago).
    let deadline = facts.overrun_attempts[0].deadline;
    assert!(
        (deadline - (now - Duration::hours(1))).num_seconds().abs() < 5,
        "{deadline}"
    );
    // The condition names it once it has been overrun for the stuck window.
    let found = evaluate(
        &Inputs {
            store: facts,
            ..Inputs::default()
        },
        &HealthThresholds {
            stuck_minutes: 30,
            ..thresholds()
        },
        &HealthAnomalySpec::default(),
        &mut Sustain::default(),
        now,
    );
    assert_eq!(
        pairs(&found),
        vec![(KIND_ATTEMPT_PAST_WALL_LIMIT, stuck.to_string())]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn an_earlier_execution_deadline_still_bounds_the_attempt_2786() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let project = Uuid::new_v4();
    let (execution, attempt) = (Uuid::new_v4(), Uuid::new_v4());
    insert_execution(
        &store,
        execution,
        Some(project),
        "running",
        &(now - Duration::hours(2)).to_rfc3339(),
        Some(&(now - Duration::minutes(10)).to_rfc3339()),
    );
    insert_attempt(&store, attempt, execution, "running");
    start_attempt(&store, attempt, now - Duration::hours(1));
    let facts = collect_store_facts(&store, &Scope::project(project), now).unwrap();
    assert_eq!(facts.overrun_attempts.len(), 1);
    assert!(
        (facts.overrun_attempts[0].deadline - (now - Duration::minutes(10)))
            .num_seconds()
            .abs()
            < 5
    );
}

// ---- #2788: a seat observes only the projects it covers ------------------------

use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::portfolio_nodes::{ConfigurePortfolioNodeRequestV1, PortfolioNodeV1};
use rsi_common::types::{Project, SessionProvider};

pub(crate) fn project_row(store: &Store) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    store
        .insert_project(&Project {
            id,
            name: format!("p {id}"),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    id
}

/// A portfolio root labelled `label` whose seat `seat` covers `projects`.
pub(crate) fn portfolio_seat(
    store: &Store,
    label: &str,
    seat: Uuid,
    projects: &[Uuid],
) -> PortfolioNodeV1 {
    store
        .insert_session(&rsid_store::test_support::test_session(
            seat,
            "/tmp/health".into(),
        ))
        .unwrap();
    store
        .configure_portfolio_node(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: None,
                adopt_node_ids: Vec::new(),
                expected_parent_grant_version: None,
                tier_label: label.into(),
                seat_session_id: seat,
                project_ids: projects.to_vec(),
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: Some("high".into()),
                }],
                policy: ManagerPolicyV2::default(),
                child_policy: None,
                max_direct_reports: 5,
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: format!("seat-{seat}"),
            },
            crate::store::portfolio_nodes::PortfolioGrantor::Operator,
            "operator:test",
        )
        .unwrap()
}

/// Narrow `node`'s grant to `projects`, the way an operator edit does.
pub(crate) fn narrow_seat(store: &Store, node: &PortfolioNodeV1, projects: &[Uuid]) {
    store
        .configure_portfolio_node(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: Some(node.node_id),
                parent_node_id: node.parent_node_id,
                adopt_node_ids: Vec::new(),
                expected_parent_grant_version: None,
                tier_label: node.tier_label.clone(),
                seat_session_id: node.grant.seat_session_id,
                project_ids: projects.to_vec(),
                allowed_launches: node.grant.allowed_launches.clone(),
                policy: node.grant.project_policy.clone(),
                child_policy: node.child_policy.clone(),
                max_direct_reports: node.max_direct_reports,
                expected_node_grant_version: node.grant.grant_version,
                expected_authority_epoch: node.authority_epoch,
                idempotency_key: format!("narrow-{}", Uuid::new_v4()),
            },
            crate::store::portfolio_nodes::PortfolioGrantor::Operator,
            "operator:test",
        )
        .unwrap();
}

fn spec_of(projects: &[Uuid]) -> HealthAnomalySpec {
    HealthAnomalySpec {
        projects: projects.to_vec(),
        ..HealthAnomalySpec::default()
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn disjoint_portfolio_seats_observe_only_their_own_projects_2788() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let (a, b) = (project_row(&store), project_row(&store));
    let (seat_a, seat_b) = (Uuid::new_v4(), Uuid::new_v4());
    portfolio_seat(&store, "swarm-a", seat_a, &[a]);
    portfolio_seat(&store, "swarm-b", seat_b, &[b]);
    let (blocked_a, blocked_b) = (Uuid::new_v4(), Uuid::new_v4());
    let ago = (now - Duration::minutes(90)).to_rfc3339();
    insert_execution(&store, blocked_a, Some(a), "blocked", &ago, None);
    insert_execution(&store, blocked_b, Some(b), "blocked", &ago, None);

    let facts_for = |seat: Uuid, stored: &[Uuid]| {
        let scope = Scope::live(&store, Some(seat), &spec_of(stored)).unwrap();
        collect_store_facts(&store, &scope, now).unwrap()
    };
    let ids = |facts: &StoreFacts| -> Vec<Uuid> {
        facts
            .blocked_executions
            .iter()
            .map(|e| e.execution_id)
            .collect()
    };
    assert_eq!(ids(&facts_for(seat_a, &[a])), vec![blocked_a]);
    assert_eq!(ids(&facts_for(seat_b, &[b])), vec![blocked_b]);
    // A stored scope claiming more than the grant is cut back to the grant.
    assert_eq!(ids(&facts_for(seat_a, &[a, b])), vec![blocked_a]);
    // A spec armed before `projects` existed follows the live grant.
    assert_eq!(ids(&facts_for(seat_b, &[])), vec![blocked_b]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_narrowed_or_revoked_grant_stops_reporting_the_removed_project_2788() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    let (a, b) = (project_row(&store), project_row(&store));
    let seat = Uuid::new_v4();
    let node = portfolio_seat(&store, "global", seat, &[a, b]);
    let (blocked_a, blocked_b) = (Uuid::new_v4(), Uuid::new_v4());
    let ago = (now - Duration::minutes(90)).to_rfc3339();
    insert_execution(&store, blocked_a, Some(a), "blocked", &ago, None);
    insert_execution(&store, blocked_b, Some(b), "blocked", &ago, None);
    // The wake was armed over both projects.
    let armed = spec_of(&[a, b]);
    let observed = |store: &Store| {
        let scope = Scope::live(store, Some(seat), &armed).unwrap();
        let mut ids: Vec<Uuid> = collect_store_facts(store, &scope, now)
            .unwrap()
            .blocked_executions
            .iter()
            .map(|e| e.execution_id)
            .collect();
        ids.sort();
        ids
    };
    let mut both = vec![blocked_a, blocked_b];
    both.sort();
    assert_eq!(observed(&store), both, "a seat covering everything sees it");

    narrow_seat(&store, &node, &[a]);
    assert_eq!(observed(&store), vec![blocked_a], "B left the grant");

    store
        .conn
        .execute("UPDATE global_manager_grants SET state='revoked'", [])
        .unwrap();
    assert!(
        observed(&store).is_empty(),
        "a revoked grant observes nothing"
    );
    assert_eq!(
        Scope::live(&store, None, &armed).unwrap(),
        Scope::none(),
        "a wake with no owner observes nothing"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_project_manager_keeps_only_its_own_project_2788() {
    let store = Store::open_in_memory().unwrap();
    let (a, b) = (project_row(&store), project_row(&store));
    let pm = Uuid::new_v4();
    let mut row = rsid_store::test_support::test_session(pm, "/tmp/health".into());
    row.project_id = Some(a);
    store.insert_session(&row).unwrap();
    let own = HealthAnomalySpec {
        project: Some(a),
        ..HealthAnomalySpec::default()
    };
    let scope = Scope::live(&store, Some(pm), &own).unwrap();
    assert_eq!(scope, Scope::project(a));
    assert!(!scope.is_fleet_wide());
    // A spec naming a project the session is not in observes nothing.
    let foreign = HealthAnomalySpec {
        project: Some(b),
        ..HealthAnomalySpec::default()
    };
    assert_eq!(
        Scope::live(&store, Some(pm), &foreign).unwrap(),
        Scope::none()
    );
}
