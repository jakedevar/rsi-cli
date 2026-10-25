//! #2747: every `when.health_anomaly` condition, injected, raises exactly one
//! condition naming its kind and subject; a quiet daemon raises none.

use super::*;
use rsi_common::health_anomaly::{
    ALL_KINDS, HealthAnomalySpec, KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
};

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
            mem_available_gb: None,
            swap_used_pct: None,
            mem_floor_gb: 0,
            build_cache_reclaim_unsupported: false,
            disk_free_gb: None,
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
                disk_wait_seconds: None,
                provider_faults: Vec::new(),
                manager_policies: Vec::new(),
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
fn a_manager_decision_pending_past_the_stuck_minutes_fires_3240() {
    let key = "topology:8c16ccf5:review:1:1".to_owned();
    let (project, execution) = (Uuid::new_v4(), Uuid::new_v4());
    let now = Utc::now();
    let mut inputs = Inputs::default();
    inputs
        .store
        .pending_manager_decisions
        .push(PendingManagerDecision {
            key: key.clone(),
            project_id: project,
            asked_at: now - Duration::minutes(29),
            execution_id: Some(execution),
        });
    let spec = HealthAnomalySpec::default();
    let at =
        |inputs: &Inputs, at| evaluate(inputs, &thresholds(), &spec, &mut Sustain::default(), at);
    assert!(at(&inputs, now).is_empty(), "29 of 30 minutes");
    let found = at(&inputs, now + Duration::minutes(2));
    assert_eq!(pairs(&found), vec![(KIND_MANAGER_DECISION_PENDING, key)]);
    assert!(found[0].detail.contains(&execution.to_string()));
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
            mem_available_gb: None,
            swap_used_pct: None,
            mem_floor_gb: 0,
            build_cache_reclaim_unsupported: false,
            disk_free_gb: None,
        },
        ..Inputs::default()
    };
    assert!(run(&host(89.9)).is_empty());
    let found = run(&host(96.0));
    assert_eq!(pairs(&found), vec![(KIND_HUB_DISK_HIGH, "/".to_owned())]);
    assert!(found[0].detail.contains("96.0%"));
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
                disk_wait_seconds: None,
                provider_faults: Vec::new(),
                manager_policies: Vec::new(),
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
fn satellite_governor_floor_and_prolonged_disk_wait_print_a_stable_ignore_key() {
    let peer = Uuid::new_v4();
    let limit = u64::from(thresholds().stuck_minutes) * 60;
    // 27 GiB on a 200 GiB disk is 86.5% used, below the percent threshold.
    // A prolonged hold also fires after disk has recovered, until the waiter polls.
    for (free, wait) in [(27_u64 << 30, None), (60_u64 << 30, Some(limit + 1))] {
        let health: SatelliteHealthV1 = serde_json::from_value(serde_json::json!({
            "daemon_version": "1.0.0", "uptime_seconds": 1,
            "sessions_running": 0, "sessions_waiting_approval": 0,
            "disk_free_bytes": 100_u64 << 30,
            "governor_disk": { "free_bytes": free, "floor_bytes": 30_u64 << 30,
                               "oldest_wait_seconds": wait }
        }))
        .unwrap();
        let inputs = Inputs {
            live: LiveHealthInputs {
                satellites: vec![SatelliteReading::from_health(peer, "laptop", &health)],
                ..LiveHealthInputs::default()
            },
            ..Inputs::default()
        };
        let found = run(&inputs);
        assert_eq!(
            pairs(&found),
            vec![(KIND_SATELLITE_DISK_LOW, peer.to_string())]
        );
        assert!(
            found[0]
                .line()
                .contains(&format!("ignore_key=satellite_disk_low:{peer}"))
        );
    }
    let mut reading = SatelliteReading {
        peer_id: peer,
        label: "laptop".into(),
        disk_free_bytes: Some(60 << 30),
        disk_floor_bytes: Some(30 << 30),
        disk_wait_seconds: Some(limit),
        manager_policies: Vec::new(),
        provider_faults: Vec::new(),
    };
    let inputs = |reading| Inputs {
        live: LiveHealthInputs {
            satellites: vec![reading],
            ..LiveHealthInputs::default()
        },
        ..Inputs::default()
    };
    assert!(
        run(&inputs(reading.clone())).is_empty(),
        "only beyond the stuck threshold"
    );
    reading.disk_wait_seconds = Some(limit + 1);
    let mut disabled = thresholds();
    disabled.stuck_minutes = 0;
    assert!(
        evaluate(
            &inputs(reading),
            &disabled,
            &HealthAnomalySpec::default(),
            &mut Sustain::default(),
            Utc::now()
        )
        .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_declared_satellite_manager_without_launches_names_its_project_and_seat() {
    use rsi_common::satellite_manager_policy::SatelliteManagerLaunchesV1;
    let peer = Uuid::new_v4();
    let project = Uuid::new_v4();
    let seat = Uuid::new_v4();
    let mut row = SatelliteManagerLaunchesV1 {
        project_id: project,
        launches: 0,
        manager_session_id: Some(seat),
        scope_version: Some(4),
        policy_version: Some(9),
        allowed_launches: Some(0),
    };
    let inputs = |row| Inputs {
        live: LiveHealthInputs {
            satellites: vec![SatelliteReading {
                peer_id: peer,
                label: "laptop".into(),
                disk_free_bytes: None,
                disk_floor_bytes: None,
                provider_faults: Vec::new(),
                manager_policies: vec![row],
                disk_wait_seconds: None,
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let found = run(&inputs(row.clone()));
    assert_eq!(
        pairs(&found),
        vec![(
            KIND_SATELLITE_MANAGER_NO_LAUNCHES,
            format!("{peer}:{project}")
        )]
    );
    assert!(found[0].detail.contains(&seat.to_string()));
    let spec = HealthAnomalySpec {
        ignore: vec![found[0].key()],
        ..Default::default()
    };
    assert!(spec.is_valid());
    assert!(
        evaluate(
            &inputs(row.clone()),
            &thresholds(),
            &spec,
            &mut Sustain::default(),
            Utc::now()
        )
        .is_empty()
    );

    row.launches = 1;
    row.allowed_launches = Some(1);
    assert!(run(&inputs(row.clone())).is_empty());
    row.launches = 0;
    assert!(run(&inputs(row.clone())).is_empty());
    row.allowed_launches = Some(0);
    row.manager_session_id = None;
    assert!(run(&inputs(row)).is_empty());
    // A row without a seat does not establish this condition.
    assert!(
        run(&inputs(SatelliteManagerLaunchesV1 {
            project_id: project,
            launches: 0,
            ..Default::default()
        }))
        .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_hot_cpu_fires_only_once_sustained() {
    let hot = Inputs {
        host: HostReading {
            disk_used_pct: None,
            cpu_c: Some(83.0),
            mem_available_gb: None,
            swap_used_pct: None,
            mem_floor_gb: 0,
            build_cache_reclaim_unsupported: false,
            disk_free_gb: None,
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
            mem_available_gb: None,
            swap_used_pct: None,
            mem_floor_gb: 0,
            build_cache_reclaim_unsupported: false,
            disk_free_gb: None,
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
        disk_wait_seconds: None,
        manager_policies: Vec::new(),
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

/// #206: a PM's creation cap warns at exactly 80% of its limit and not below,
/// names the cap, used/limit and the refusal's raise path, and stays quiet once
/// its `ignore_key` is accepted. A zero limit (no creations at all) never warns.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_creation_cap_at_eighty_percent_names_the_cap_and_its_raise_path() {
    let project = Uuid::new_v4();
    let raise_in = "TUI: gm (global manager workspace) > b Budget > project 12345678 > created sessions > + or type a number";
    let inputs = |field: &str, used: i64, limit: i64| {
        let mut inputs = Inputs::default();
        inputs.store.creation_caps.push(CreationCapFact {
            project_id: project,
            cap: CreationCapUsageV1 {
                cap_field: field.into(),
                used,
                limit,
                raise_in: raise_in.into(),
                window_hours: None,
                next_free_at: None,
                lifetime_total: None,
            },
        });
        inputs
    };
    // 51 of 64 is 79.7%: quiet. 52 of 64 (81.2%) and 8 of 10 (exactly 80%) fire.
    assert!(run(&inputs("max_created_sessions", 51, 64)).is_empty());
    assert!(run(&inputs("max_created_sessions", 7, 10)).is_empty());
    assert!(run(&inputs("max_created_sessions", 0, 0)).is_empty());
    let subject = format!("{project}:max_created_sessions");
    for (used, limit) in [(8, 10), (52, 64), (64, 64)] {
        let found = run(&inputs("max_created_sessions", used, limit));
        assert_eq!(
            pairs(&found),
            vec![(KIND_MANAGER_CREATION_CAP_NEAR, subject.clone())]
        );
        assert_eq!(
            found[0].detail,
            format!(
                "cap=max_created_sessions used={used} limit={limit} warn_at=80% raise_in={raise_in}"
            )
        );
    }
    let containers = run(&inputs("max_created_containers", 4, 5));
    assert_eq!(
        pairs(&containers),
        vec![(
            KIND_MANAGER_CREATION_CAP_NEAR,
            format!("{project}:max_created_containers")
        )]
    );

    // Accepting the printed ignore_key silences exactly that cap.
    let key = format!("{KIND_MANAGER_CREATION_CAP_NEAR}:{subject}");
    assert_eq!(run(&inputs("max_created_sessions", 8, 10))[0].key(), key);
    let spec = HealthAnomalySpec {
        ignore: vec![key],
        ..HealthAnomalySpec::default()
    };
    assert!(
        spec.is_valid(),
        "the printed key is an accepted ignore entry"
    );
    let ignored = evaluate(
        &inputs("max_created_sessions", 9, 10),
        &thresholds(),
        &spec,
        &mut Sustain::default(),
        Utc::now(),
    );
    assert!(ignored.is_empty(), "{ignored:?}");
    let other = evaluate(
        &inputs("max_created_containers", 9, 10),
        &thresholds(),
        &spec,
        &mut Sustain::default(),
        Utc::now(),
    );
    assert_eq!(other.len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn an_open_helper_llm_breaker_raises_one_condition_naming_the_setting_and_the_fix() {
    // #261: a helper endpoint that is not listening is one condition per
    // endpoint, naming the purpose, the setting key, the model and the fix.
    let outage = HelperOutage {
        endpoint: "localhost:11434".into(),
        purpose: "session.summary".into(),
        setting_key: "memory_model_fallback_provider".into(),
        model: "gemma4:e4b".into(),
        other_purposes: vec!["session.title(title_model_local)".into()],
        since: Utc::now(),
        next_probe_in_secs: 120,
        failed_probes: 1,
    };
    let inputs = Inputs {
        helper_outages: vec![outage],
        ..Inputs::default()
    };
    let found = run(&inputs);
    assert_eq!(
        pairs(&found),
        vec![(KIND_HELPER_LLM_UNREACHABLE, "localhost:11434".to_owned())]
    );
    let detail = &found[0].detail;
    assert!(detail.contains("purpose=session.summary"), "{detail}");
    assert!(
        detail.contains("setting=memory_model_fallback_provider"),
        "{detail}"
    );
    assert!(detail.contains("model=gemma4:e4b"), "{detail}");
    assert!(
        detail.contains("also=session.title(title_model_local)"),
        "{detail}"
    );
    assert!(detail.contains("ollama pull gemma4:e4b"), "{detail}");
    // The printed key is a valid ignore key.
    let spec = HealthAnomalySpec {
        ignore: vec![found[0].key()],
        ..HealthAnomalySpec::default()
    };
    assert!(spec.is_valid());
    // A closed breaker raises nothing.
    assert!(run(&Inputs::default()).is_empty());
}

fn daemon_stop(minutes_ago: i64, reason: &str, active: &[&str]) -> DaemonStop {
    let stopped_at = Utc::now() - Duration::minutes(minutes_ago);
    DaemonStop {
        started_at: Some(stopped_at - Duration::minutes(4)),
        stopped_at,
        reason: reason.to_owned(),
        active: active.iter().map(|unit| (*unit).to_owned()).collect(),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn three_unannounced_daemon_stops_in_two_hours_raise_a_restart_loop_3480() {
    // The 2026-10-21 pattern: SIGINT stops about four minutes after each
    // start, the same lander entry gating at every one.
    let inputs = Inputs {
        daemon_stops: vec![
            daemon_stop(90, "ctrl_c", &["lander:entry=e1:source=abc"]),
            daemon_stop(50, "ctrl_c", &["lander:entry=e1:source=abc"]),
            daemon_stop(
                10,
                "ctrl_c",
                &["lander:entry=e1:source=abc", "job:test:rsi-job-9"],
            ),
        ],
        ..Inputs::default()
    };
    let found = run(&inputs);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, KIND_DAEMON_RESTART_LOOP);
    assert!(found[0].subject.starts_with("hub:"), "{}", found[0].subject);
    let detail = &found[0].detail;
    assert!(detail.contains("unannounced_stops=3"), "{detail}");
    assert!(detail.contains("uptime=240s"), "{detail}");
    assert!(detail.contains("reason=ctrl_c"), "{detail}");
    assert!(detail.contains("lander:entry=e1:source=abc"), "{detail}");
    assert!(detail.contains("job:test:rsi-job-9"), "{detail}");
    // The printed key is a valid ignore key, and a newer stop is heard again.
    let spec = HealthAnomalySpec {
        ignore: vec![found[0].key()],
        ..HealthAnomalySpec::default()
    };
    assert!(spec.is_valid());
    assert!(
        evaluate(
            &inputs,
            &thresholds(),
            &spec,
            &mut Sustain::default(),
            Utc::now()
        )
        .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn announced_old_or_too_few_daemon_stops_raise_no_restart_loop_3480() {
    // Two unannounced stops are not a loop.
    let two = Inputs {
        daemon_stops: vec![
            daemon_stop(30, "sigterm", &[]),
            daemon_stop(10, "unrecorded", &[]),
        ],
        ..Inputs::default()
    };
    assert!(run(&two).is_empty());
    // Operator deploy drains are announced and never counted.
    let drains = Inputs {
        daemon_stops: vec![
            daemon_stop(60, "drain_restart", &[]),
            daemon_stop(40, "drain_restart", &[]),
            daemon_stop(20, "drain_restart", &[]),
            daemon_stop(5, "sigterm", &[]),
        ],
        ..Inputs::default()
    };
    assert!(run(&drains).is_empty());
    // A stop older than two hours falls out of the window.
    let old = Inputs {
        daemon_stops: vec![
            daemon_stop(300, "ctrl_c", &[]),
            daemon_stop(20, "ctrl_c", &[]),
            daemon_stop(10, "ctrl_c", &[]),
        ],
        ..Inputs::default()
    };
    assert!(run(&old).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_spent_supervisor_restart_budget_raises_an_anomaly_and_clears_with_the_window_3492() {
    use rsi_common::supervisor_restarts::SupervisorRestarts;
    let now = Utc::now().timestamp();
    let published = |restarts: &[i64], backoff_until: Option<i64>| Inputs {
        supervisor_restarts: Some(SupervisorRestarts {
            limit: 3,
            window_secs: 3600,
            restarts: restarts.to_vec(),
            backoff_until,
        }),
        ..Inputs::default()
    };
    // Two crashes and a deploy fill the budget of three: a finding.
    let spent = published(&[now - 900, now - 600, now - 30], None);
    let found = run(&spent);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, KIND_SUPERVISOR_RESTART_BUDGET);
    assert_eq!(found[0].subject, format!("hub:{}", now - 30));
    assert!(
        found[0].detail.contains("restarts_in_window=3 limit=3"),
        "{}",
        found[0].detail
    );
    assert!(
        found[0]
            .detail
            .contains(&format!("frees_at_unix={}", now - 900 + 3600)),
        "{}",
        found[0].detail
    );
    // Room left: nothing fires; a back-off in progress fires whatever the count.
    assert!(run(&published(&[now - 900, now - 600], None)).is_empty());
    let backing_off = run(&published(&[now - 30], Some(now + 300)));
    assert_eq!(backing_off.len(), 1, "{backing_off:?}");
    assert!(
        backing_off[0].detail.contains("backing_off=true"),
        "{}",
        backing_off[0].detail
    );
    // Restarts that left the window no longer count, and no file means no anomaly.
    assert!(run(&published(&[now - 9000, now - 8000, now - 7000], None)).is_empty());
    assert!(run(&Inputs::default()).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn every_kind_is_covered_by_an_injection_test_above() {
    // The kinds this module and satellite_seats::tests inject. A new kind must add a test
    // and be listed here, so no condition ships unproved.
    let injected = [
        KIND_PM_SEAT_BLOCKED,
        KIND_PM_CONTEXT_FULL,
        KIND_EXECUTION_BLOCKED,
        KIND_ATTEMPT_PAST_WALL_LIMIT,
        KIND_HUB_DISK_HIGH,
        KIND_HUB_DISK_CRITICAL,
        KIND_HUB_NOT_PULLING,
        KIND_SATELLITE_DISK_LOW,
        KIND_SATELLITE_MANAGER_NO_LAUNCHES,
        rsi_common::health_anomaly::KIND_SATELLITE_MANAGER_SEAT_FAILED,
        KIND_CPU_HOT,
        KIND_CPU_CRITICAL,
        KIND_PROVIDER_MODEL_UNAVAILABLE,
        KIND_PROVIDER_AUTH_FAILURE,
        KIND_ROLLING_QA_RED,
        KIND_RECLAIM_UNSUPPORTED,
        KIND_MANAGER_DECISION_PENDING,
        KIND_MANAGER_CREATION_CAP_NEAR,
        KIND_HELPER_LLM_UNREACHABLE,
        KIND_HUB_MEMORY_HIGH,
        KIND_DAEMON_RESTART_LOOP,
        KIND_SUPERVISOR_RESTART_BUDGET,
        KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
    ];
    assert_eq!(injected.len(), ALL_KINDS.len());
    for kind in ALL_KINDS {
        assert!(injected.contains(kind), "{kind}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_stale_hub_pull_fires_and_recovery_clears() {
    use rsi_common::satellite_dispatch::SatelliteReportBacklogV1;
    let now = Utc::now();
    let hub = Uuid::new_v4();
    let mut inputs = Inputs::default();
    let check = |inputs: &Inputs, spec: &HealthAnomalySpec| {
        evaluate(inputs, &thresholds(), spec, &mut Sustain::default(), now)
    };
    let spec = HealthAnomalySpec::default();
    // Hub / unpaired satellite: no inbound link, so nothing fires.
    assert!(check(&inputs, &spec).is_empty());
    inputs.store.hub_link = Some(HubReportLinkStatus {
        hub_installations: vec![hub],
        reports: SatelliteReportStatusV1 {
            last_hub_pull_at: Some((now - Duration::seconds(HUB_PULL_STALE_SECS + 1)).to_rfc3339()),
            backlog: SatelliteReportBacklogV1 {
                depth: 3,
                oldest_queued_at: Some((now - Duration::seconds(1000)).to_rfc3339()),
                oldest_age_secs: Some(1000),
                ..Default::default()
            },
            ..Default::default()
        },
    });
    let found = check(&inputs, &spec);
    assert_eq!(pairs(&found), vec![(KIND_HUB_NOT_PULLING, hub.to_string())]);
    assert!(found[0].detail.contains("last_hub_pull_at="));
    assert!(found[0].detail.contains("backlog_depth=3"));
    assert!(found[0].detail.contains("oldest_age_secs=Some(1000)"));
    let ignored = HealthAnomalySpec {
        ignore: vec![found[0].key()],
        ..Default::default()
    };
    assert!(ignored.is_valid());
    assert!(check(&inputs, &ignored).is_empty());
    // The missed heartbeat also matters when no report happens to be pending.
    inputs.store.hub_link.as_mut().unwrap().reports.backlog = Default::default();
    assert_eq!(
        pairs(&check(&inputs, &spec)),
        vec![(KIND_HUB_NOT_PULLING, hub.to_string())]
    );
    // Resumed pulls clear immediately, even while old reports remain pending.
    let link = inputs.store.hub_link.as_mut().unwrap();
    link.reports.backlog.oldest_queued_at = Some((now - Duration::hours(1)).to_rfc3339());
    link.reports.last_hub_pull_at = Some(now.to_rfc3339());
    assert!(check(&inputs, &spec).is_empty());
    // Exactly the deadline is still fresh; it fires only once older.
    inputs
        .store
        .hub_link
        .as_mut()
        .unwrap()
        .reports
        .last_hub_pull_at = Some((now - Duration::seconds(HUB_PULL_STALE_SECS)).to_rfc3339());
    assert!(check(&inputs, &spec).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn rearming_hub_pull_health_notifies_once_per_manager_and_outage() {
    let now = Utc::now();
    let hub = Uuid::new_v4();
    let manager = Uuid::new_v4();
    let first_wake = Uuid::new_v4();
    let mut inputs = Inputs::default();
    inputs.store.hub_link = Some(HubReportLinkStatus {
        hub_installations: vec![hub],
        reports: SatelliteReportStatusV1 {
            last_hub_pull_at: Some((now - Duration::seconds(HUB_PULL_STALE_SECS + 1)).to_rfc3339()),
            ..Default::default()
        },
    });
    let mut outages = HubPullOutages::default();
    // Each call represents a newly armed health_anomaly: {} with a new wake
    // and fresh sustain clocks. Only the outage latch survives the old wake.
    let mut check = |owner, wake, inputs: &Inputs, at| {
        outages.evaluate(
            (owner, wake),
            inputs,
            &thresholds(),
            &HealthAnomalySpec::default(),
            &mut Sustain::default(),
            at,
        )
    };
    let expected = vec![(KIND_HUB_NOT_PULLING, hub.to_string())];
    assert_eq!(pairs(&check(manager, first_wake, &inputs, now)), expected);
    assert!(check(manager, Uuid::new_v4(), &inputs, now).is_empty());
    // A refused delivery can retry its original wake without losing the alert.
    assert_eq!(pairs(&check(manager, first_wake, &inputs, now)), expected);
    // One observer acknowledging does not silence another manager.
    assert_eq!(
        pairs(&check(Uuid::new_v4(), Uuid::new_v4(), &inputs, now)),
        expected
    );
    inputs
        .store
        .hub_link
        .as_mut()
        .unwrap()
        .reports
        .last_hub_pull_at = Some(now.to_rfc3339());
    let second_wake = Uuid::new_v4();
    assert!(check(manager, second_wake, &inputs, now).is_empty());
    let later = now + Duration::seconds(HUB_PULL_STALE_SECS + 1);
    assert_eq!(
        pairs(&check(manager, second_wake, &inputs, later)),
        expected
    );
    assert!(check(manager, Uuid::new_v4(), &inputs, later).is_empty());
    // A successful pull between evaluations also starts a distinct outage.
    inputs
        .store
        .hub_link
        .as_mut()
        .unwrap()
        .reports
        .last_hub_pull_at = Some(later.to_rfc3339());
    let third = later + Duration::seconds(HUB_PULL_STALE_SECS + 1);
    assert_eq!(
        pairs(&check(manager, Uuid::new_v4(), &inputs, third)),
        expected
    );
    assert!(check(manager, Uuid::new_v4(), &inputs, third).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn hub_pull_outage_latches_are_per_hub_and_clear_on_unpairing() {
    let now = Utc::now();
    let owner = Uuid::new_v4();
    let hub = Uuid::new_v4();
    let other_hub = Uuid::new_v4();
    let mut inputs = Inputs::default();
    inputs.store.hub_link = Some(HubReportLinkStatus {
        hub_installations: vec![hub],
        reports: SatelliteReportStatusV1 {
            last_hub_pull_at: Some((now - Duration::hours(1)).to_rfc3339()),
            ..Default::default()
        },
    });
    let mut outages = HubPullOutages::default();
    let mut check = |inputs: &Inputs| {
        outages.evaluate(
            (owner, Uuid::new_v4()),
            inputs,
            &thresholds(),
            &HealthAnomalySpec::default(),
            &mut Sustain::default(),
            now,
        )
    };
    assert_eq!(
        pairs(&check(&inputs)),
        vec![(KIND_HUB_NOT_PULLING, hub.to_string())]
    );
    inputs
        .store
        .hub_link
        .as_mut()
        .unwrap()
        .hub_installations
        .push(other_hub);
    assert_eq!(
        pairs(&check(&inputs)),
        vec![(KIND_HUB_NOT_PULLING, other_hub.to_string())]
    );
    let link = inputs.store.hub_link.take();
    assert!(check(&inputs).is_empty());
    inputs.store.hub_link = link;
    assert_eq!(
        pairs(&check(&inputs)),
        vec![
            (KIND_HUB_NOT_PULLING, hub.to_string()),
            (KIND_HUB_NOT_PULLING, other_hub.to_string()),
        ]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn hub_pull_health_before_the_first_pull_needs_an_aged_report() {
    let now = Utc::now();
    let mut status = SatelliteReportStatusV1::default();
    assert!(!hub_pull_stale(&status, now));
    status.backlog.oldest_queued_at = Some(now.to_rfc3339());
    assert!(!hub_pull_stale(&status, now));
    status.backlog.oldest_queued_at =
        Some((now - Duration::seconds(HUB_PULL_STALE_SECS + 1)).to_rfc3339());
    assert!(hub_pull_stale(&status, now));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn project_health_collects_its_inbound_hub_link_and_drops_it_on_unpairing() {
    use rsi_common::satellite::SatelliteUuidV1;
    use rsi_common::satellite_dispatch::SatelliteInboundPolicyV1;
    let store = Store::open_in_memory().unwrap();
    let project = project_row(&store);
    let scope = Scope::project(project);
    let hub = Uuid::new_v4();
    let now = Utc::now();
    assert_eq!(
        collect_store_facts(&store, &scope, now).unwrap().hub_link,
        None
    );
    store
        .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
            allowed_hub_installations: vec![SatelliteUuidV1(hub)],
            scope_projects: vec![SatelliteUuidV1(project)],
            ..Default::default()
        })
        .unwrap();
    // This is the same store path reached by the wire FetchHubReports handler.
    store.take_hub_reports(hub, &[], &[]).unwrap();
    let at = Utc::now() + Duration::seconds(HUB_PULL_STALE_SECS + 1);
    let facts = collect_store_facts(&store, &scope, at).unwrap();
    let link = facts.hub_link.as_ref().unwrap();
    assert_eq!(link.hub_installations, vec![hub]);
    assert!(hub_pull_stale(&link.reports, at));
    assert_eq!(
        collect_store_facts(&store, &Scope::none(), at)
            .unwrap()
            .hub_link,
        None
    );
    store
        .put_satellite_inbound_policy(&SatelliteInboundPolicyV1::default())
        .unwrap();
    assert_eq!(
        collect_store_facts(&store, &scope, at).unwrap().hub_link,
        None
    );
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
    row.session_kind = rsi_common::types::SessionKind::Standard;
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    let own = HealthAnomalySpec {
        project: Some(a),
        ..HealthAnomalySpec::default()
    };
    assert_eq!(Scope::live(&store, Some(pm), &own).unwrap(), Scope::none());
    store
        .configure_harness_manager(
            &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                project_id: a,
                session_id: pm,
                epic_ids: None,
                group_ids: vec![],
                expected_row_version: 0,
            },
        )
        .unwrap();
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
    store
        .configure_harness_manager(
            &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                project_id: a,
                session_id: pm,
                epic_ids: Some(vec![]),
                group_ids: vec![],
                expected_row_version: 1,
            },
        )
        .unwrap();
    assert_eq!(Scope::live(&store, Some(pm), &own).unwrap(), Scope::none());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn area_health_filters_blocked_executions_and_overruns_to_direct_epics() {
    let store = Store::open_in_memory().unwrap();
    let project = project_row(&store);
    let selected = Uuid::new_v4();
    let sibling = Uuid::new_v4();
    let now = Utc::now();
    for epic in [selected, sibling] {
        insert_session(&store, epic, "Claude", "test", "", now);
        store
            .conn
            .execute(
                "UPDATE sessions SET session_kind='Epic',project_id=?2 WHERE id=?1",
                params![epic.to_string(), project.to_string()],
            )
            .unwrap();
    }
    let ago = (now - Duration::hours(8)).to_rfc3339();
    let mut expected = Vec::new();
    for epic in [Some(selected), Some(sibling), None] {
        for status in ["blocked", "running"] {
            let execution = Uuid::new_v4();
            insert_execution(&store, execution, Some(project), status, &ago, Some(&ago));
            store
                .conn
                .execute(
                    "UPDATE topology_executions SET epic_id=?2 WHERE id=?1",
                    params![execution.to_string(), epic.map(|id| id.to_string())],
                )
                .unwrap();
            if status == "running" {
                insert_attempt(&store, Uuid::new_v4(), execution, "running");
            }
            if epic == Some(selected) {
                expected.push(execution);
            }
        }
    }
    let scope = Scope {
        projects: vec![project],
        epic_ids: Some(vec![selected]),
        ..Scope::default()
    };
    let facts = collect_store_facts(&store, &scope, now).unwrap();
    assert_eq!(
        facts
            .blocked_executions
            .iter()
            .map(|e| e.execution_id)
            .collect::<Vec<_>>(),
        vec![expected[0]]
    );
    assert_eq!(
        facts
            .overrun_attempts
            .iter()
            .map(|a| a.execution_id)
            .collect::<Vec<_>>(),
        vec![expected[1]]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn hub_memory_fires_on_low_available_memory_or_nearly_full_swap() {
    let host = |available: Option<f64>, swap: Option<f64>, floor| Inputs {
        host: HostReading {
            mem_available_gb: available,
            swap_used_pct: swap,
            mem_floor_gb: floor,
            ..HostReading::default()
        },
        ..Inputs::default()
    };
    // Floor 16 GiB: the available arm fires strictly below 8 GiB.
    assert!(run(&host(Some(8.0), Some(10.0), 16)).is_empty());
    let low = run(&host(Some(7.9), None, 16));
    assert_eq!(pairs(&low), vec![(KIND_HUB_MEMORY_HIGH, "hub".to_owned())]);
    assert!(low[0].detail.contains("mem_available=7.9GiB"));
    // Swap arm: 80% and above fires whatever MemAvailable says.
    assert!(run(&host(Some(40.0), Some(79.9), 16)).is_empty());
    let swap = run(&host(Some(40.0), Some(100.0), 16));
    assert_eq!(pairs(&swap), vec![(KIND_HUB_MEMORY_HIGH, "hub".to_owned())]);
    assert!(swap[0].detail.contains("swap_used=100.0%"));
    // Both arms hold: still one condition. A zero floor turns only the
    // available arm off; an unreadable source never fires.
    assert_eq!(run(&host(Some(1.0), Some(95.0), 16)).len(), 1);
    assert!(run(&host(Some(1.0), None, 0)).is_empty());
    assert_eq!(run(&host(Some(1.0), Some(95.0), 0)).len(), 1);
    assert!(run(&host(None, None, 16)).is_empty());
}

fn host_reading(used: Option<f64>, free: Option<f64>, cpu: Option<f64>) -> Inputs {
    Inputs {
        host: HostReading {
            disk_used_pct: used,
            cpu_c: cpu,
            build_cache_reclaim_unsupported: false,
            disk_free_gb: free,
            ..HostReading::default()
        },
        ..Inputs::default()
    }
}

/// An owner who accepted the warm keys, as the wake message printed them.
fn warm_ignored() -> HealthAnomalySpec {
    HealthAnomalySpec {
        ignore: vec![
            format!("{KIND_HUB_DISK_HIGH}:/"),
            format!("{KIND_CPU_HOT}:hub"),
        ],
        ..HealthAnomalySpec::default()
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_warm_disk_ignore_still_hides_warm_and_a_critical_disk_still_wakes() {
    let spec = warm_ignored();
    let eval = |inputs: &Inputs| {
        evaluate(
            inputs,
            &thresholds(),
            &spec,
            &mut Sustain::default(),
            Utc::now(),
        )
    };
    // The accepted warm condition (90.5% used, 95 GB free) stays quiet.
    assert!(eval(&host_reading(Some(90.5), Some(95.0), None)).is_empty());
    // Used >= 97% wakes despite the warm ignore, with its own key and severity.
    let found = eval(&host_reading(Some(98.0), Some(60.0), None));
    assert_eq!(
        pairs(&found),
        vec![(KIND_HUB_DISK_CRITICAL, "/".to_owned())]
    );
    assert!(found[0].detail.contains("severity=critical"));
    assert!(found[0].line().contains("ignore_key=hub_disk_critical:/ "));
    // Free space under 20 GB is critical even at a warm percentage.
    let found = eval(&host_reading(Some(91.0), Some(19.0), None));
    assert_eq!(
        pairs(&found),
        vec![(KIND_HUB_DISK_CRITICAL, "/".to_owned())]
    );
    assert!(found[0].detail.contains("free=19.0GiB"));
    // Critical is one line: it replaces the warm one, and an owner who also
    // accepted the critical key hears nothing.
    let both = HealthAnomalySpec {
        ignore: vec![format!("{KIND_HUB_DISK_CRITICAL}:/")],
        ..HealthAnomalySpec::default()
    };
    assert!(
        evaluate(
            &host_reading(Some(98.0), Some(5.0), None),
            &thresholds(),
            &both,
            &mut Sustain::default(),
            Utc::now()
        )
        .is_empty()
    );
    // An unwarmed, unignored warm condition keeps its warm kind and severity.
    let found = run(&host_reading(Some(91.0), Some(60.0), None));
    assert_eq!(pairs(&found), vec![(KIND_HUB_DISK_HIGH, "/".to_owned())]);
    assert!(found[0].detail.contains("severity=warm"));
    // The disk check being off silences critical too.
    let off = HealthThresholds {
        disk_used_pct: 0,
        ..thresholds()
    };
    assert!(
        evaluate(
            &host_reading(Some(99.0), Some(1.0), None),
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
fn provider_window_projection_reads_the_shared_account_windows() {
    use rsi_common::rpc::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
    let store = Store::open_in_memory().unwrap();
    let now = DateTime::parse_from_rfc3339("2026-10-22T04:54:40Z")
        .unwrap()
        .with_timezone(&Utc);
    let reset = DateTime::parse_from_rfc3339("2026-10-26T13:00:00Z")
        .unwrap()
        .timestamp();
    store
        .upsert_provider_rate_limit_snapshot(
            &ProviderRateLimitSnapshot {
                provider: rsi_common::types::SessionProvider::Claude,
                status: Some("allowed_warning".into()),
                rate_limit_type: Some("seven_day".into()),
                overage_status: Some("rejected".into()),
                is_using_overage: false,
                observed_at: now,
                stale_reason: None,
                windows: vec![ProviderRateLimitWindow {
                    window_key: "seven_day".into(),
                    utilization: 0.83,
                    resets_at_epoch: Some(reset),
                }],
            },
            None,
        )
        .unwrap();
    let facts = collect_store_facts(&store, &Scope::project(Uuid::new_v4()), now).unwrap();
    let inputs = Inputs {
        store: facts,
        ..Inputs::default()
    };
    let mut tracker = Sustain::default();
    let found = evaluate(
        &inputs,
        &thresholds(),
        &HealthAnomalySpec::default(),
        &mut tracker,
        now,
    );
    assert_eq!(
        pairs(&found),
        vec![(
            KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
            format!("Claude:seven_day:{reset}")
        )]
    );
    assert!(found[0].detail.contains("utilization=83.0%"));
    assert!(
        found[0]
            .detail
            .contains("resets_at=2026-10-26T13:00:00+00:00")
    );
    assert!(
        found[0]
            .detail
            .contains("projected_exhaustion=2026-10-22T18:00:")
    );
    tracker.windows.reported_subject(found[0].subject.clone());
    assert!(
        evaluate(
            &inputs,
            &thresholds(),
            &HealthAnomalySpec::default(),
            &mut tracker,
            now
        )
        .is_empty(),
        "same delivered episode is reported once"
    );
    assert!(
        collect_store_facts(&store, &Scope::none(), now)
            .unwrap()
            .provider_windows
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
fn projection_snapshot(
    now: DateTime<Utc>,
    reset: DateTime<Utc>,
    used: f64,
) -> rsi_common::rpc::ProviderRateLimitSnapshot {
    rsi_common::rpc::ProviderRateLimitSnapshot {
        provider: rsi_common::types::SessionProvider::Codex,
        status: None,
        rate_limit_type: None,
        overage_status: None,
        is_using_overage: false,
        observed_at: now,
        stale_reason: None,
        windows: vec![rsi_common::rpc::ProviderRateLimitWindow {
            window_key: "seven_day".into(),
            utilization: used,
            resets_at_epoch: Some(reset.timestamp()),
        }],
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn projected_window_respects_threshold_reset_and_unusable_telemetry() {
    let now = Utc::now();
    let reset = now + Duration::days(4);
    let baseline = projection_snapshot(now, reset, 0.83);
    let mut tracker = provider_windows::WindowTracker::default();
    assert_eq!(tracker.evaluate(&[baseline.clone()], 70, now).len(), 1);
    assert!(tracker.evaluate(&[baseline.clone()], 0, now).is_empty());
    assert!(tracker.evaluate(&[baseline.clone()], 84, now).is_empty());
    let mut benign = baseline.clone();
    benign.windows[0].resets_at_epoch = Some((now + Duration::hours(5)).timestamp());
    assert!(
        tracker.evaluate(&[benign], 70, now).is_empty(),
        "pace fits remaining quota"
    );
    for used in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        let mut invalid = baseline.clone();
        invalid.windows[0].utilization = used;
        assert!(tracker.evaluate(&[invalid], 70, now).is_empty());
    }
    for reset in [
        None,
        Some(now.timestamp()),
        Some(i64::MAX),
        Some((now + Duration::days(8)).timestamp()),
    ] {
        let mut invalid = baseline.clone();
        invalid.windows[0].resets_at_epoch = reset;
        assert!(tracker.evaluate(&[invalid], 70, now).is_empty());
    }
    let mut unknown = baseline.clone();
    unknown.windows[0].window_key = "unknown".into();
    assert!(tracker.evaluate(&[unknown], 70, now).is_empty());
    let mut stale = baseline.clone();
    stale.stale_reason = Some("probe_timeout".into());
    assert!(tracker.evaluate(&[stale], 70, now).is_empty());
    let mut future = baseline;
    future.observed_at = now + Duration::hours(1);
    assert!(tracker.evaluate(&[future], 70, now).is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn recent_burn_warns_when_the_window_average_would_fit() {
    let now = Utc::now();
    let reset = now + Duration::days(1);
    let yesterday = projection_snapshot(now - Duration::hours(24), reset, 0.2);
    let today = projection_snapshot(now, reset, 0.7);
    let mut tracker = provider_windows::WindowTracker::default();
    assert!(
        tracker
            .evaluate(&[yesterday], 70, now - Duration::hours(24))
            .is_empty()
    );
    let found = tracker.evaluate(&[today.clone()], 70, now);
    assert_eq!(found.len(), 1);
    assert!(found[0].detail.contains("burn_basis=recent_up_to_24h"));
    assert!(
        provider_windows::WindowTracker::default()
            .evaluate(&[today], 70, now)
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_warm_cpu_ignore_still_hides_warm_and_a_critical_cpu_still_wakes() {
    let spec = warm_ignored();
    let start = Utc::now();
    let sustained = start + Duration::seconds(THERMAL_SUSTAIN_SECS);
    let eval = |cpu: f64| {
        let inputs = host_reading(None, None, Some(cpu));
        let mut tracker = Sustain::default();
        assert!(evaluate(&inputs, &thresholds(), &spec, &mut tracker, start).is_empty());
        evaluate(&inputs, &thresholds(), &spec, &mut tracker, sustained)
    };
    // The accepted 84.5 C condition stays quiet.
    assert!(eval(84.5).is_empty());
    // 95 C wakes despite the warm ignore, under its own key.
    let found = eval(95.0);
    assert_eq!(pairs(&found), vec![(KIND_CPU_CRITICAL, "hub".to_owned())]);
    assert!(found[0].detail.contains("severity=critical"));
    assert!(found[0].line().contains("ignore_key=cpu_critical:hub "));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn acknowledged_window_episodes_dedupe_and_new_resets_warn() {
    let now = Utc::now();
    let reset = now + Duration::days(4);
    let baseline = projection_snapshot(now, reset, 0.83);
    let mut tracker = Sustain::default();
    let mut inputs = Inputs::default();
    inputs.store.provider_windows = vec![baseline];
    let spec = HealthAnomalySpec::default();
    let found = evaluate(&inputs, &thresholds(), &spec, &mut tracker, now);
    assert_eq!(found.len(), 1);
    // A held/refused delivery has not acknowledged anything.
    assert_eq!(
        evaluate(&inputs, &thresholds(), &spec, &mut tracker, now),
        found
    );
    let ignored = HealthAnomalySpec {
        ignore: vec![found[0].key()],
        ..spec.clone()
    };
    assert!(evaluate(&inputs, &thresholds(), &ignored, &mut tracker, now).is_empty());
    tracker.windows.reported_subject(found[0].subject.clone());
    assert!(evaluate(&inputs, &thresholds(), &spec, &mut tracker, now).is_empty());
    inputs.store.provider_windows[0].windows[0].resets_at_epoch =
        Some((reset + Duration::hours(1)).timestamp());
    let next = evaluate(&inputs, &thresholds(), &ignored, &mut tracker, now);
    assert_eq!(next.len(), 1);
    assert_ne!(next[0].subject, found[0].subject);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn accepted_window_epochs_tolerate_jitter_across_time_buckets_without_chaining() {
    // Include #707's actual epochs and an exact hour/minute boundary. A
    // floor/round-to-minute fix would still split one of these pairs.
    let boundary = DateTime::parse_from_rfc3339("2026-10-26T13:00:00Z")
        .unwrap()
        .timestamp();
    for epoch in [1_793_382_080, boundary] {
        let reset = DateTime::from_timestamp(epoch, 0).unwrap();
        let now = reset - Duration::days(4);
        let mut inputs = Inputs::default();
        inputs.store.provider_windows = vec![projection_snapshot(now, reset, 0.83)];
        let found = evaluate(
            &inputs,
            &thresholds(),
            &HealthAnomalySpec::default(),
            &mut Sustain::default(),
            now,
        );
        assert_eq!(found.len(), 1);
        let legacy_key =
            format!("{KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION}:Codex:seven_day:{epoch}");
        assert_eq!(found[0].key(), legacy_key);
        let line = found[0].line();
        let printed_key = line
            .split_once(" ignore_key=")
            .unwrap()
            .1
            .split_once(" detail=")
            .unwrap()
            .0;
        assert_eq!(printed_key, legacy_key);
        // An already-armed, serialized epoch ignore needs no rewrite.
        let ignored: HealthAnomalySpec =
            serde_json::from_value(serde_json::json!({"ignore": [printed_key]})).unwrap();
        assert!(ignored.is_valid());
        let mut restored = Sustain::default();
        restored.windows.reported_subject(found[0].subject.clone());
        for delta in [-60, -1, 0, 1, 60] {
            inputs.store.provider_windows[0].windows[0].resets_at_epoch = Some(epoch + delta);
            assert!(
                evaluate(
                    &inputs,
                    &thresholds(),
                    &ignored,
                    &mut Sustain::default(),
                    now,
                )
                .is_empty(),
                "accepted reset={epoch}, jitter={delta}"
            );
            // Rehydrated delivery receipts use the identical bounded match.
            assert!(
                evaluate(
                    &inputs,
                    &thresholds(),
                    &HealthAnomalySpec::default(),
                    &mut restored,
                    now,
                )
                .is_empty(),
                "delivered reset={epoch}, jitter={delta}"
            );
        }
        // Observing +60 does not move the accepted anchor to +60.
        inputs.store.provider_windows[0].windows[0].resets_at_epoch = Some(epoch + 61);
        let outside = evaluate(&inputs, &thresholds(), &ignored, &mut restored, now);
        assert_eq!(
            pairs(&outside),
            vec![(
                KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
                format!("Codex:seven_day:{}", epoch + 61),
            )]
        );
        let next_now = now + Duration::days(7);
        inputs.store.provider_windows = vec![projection_snapshot(
            next_now,
            reset + Duration::days(7),
            0.83,
        )];
        let next = evaluate(&inputs, &thresholds(), &ignored, &mut restored, next_now);
        assert_eq!(
            pairs(&next),
            vec![(
                KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
                format!("Codex:seven_day:{}", epoch + 7 * 86400),
            )]
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn window_epoch_ignores_keep_provider_window_and_condition_discrimination() {
    use rsi_common::types::SessionProvider;
    let now = Utc::now();
    let reset = now + Duration::days(4);
    let accepted = format!(
        "{KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION}:Codex:seven_day:{}",
        reset.timestamp()
    );
    let spec = HealthAnomalySpec {
        ignore: vec![accepted],
        ..HealthAnomalySpec::default()
    };
    let mut inputs = Inputs::default();
    let baseline = projection_snapshot(now, reset, 0.83);
    for (provider, accepted_window, key) in [
        (SessionProvider::Claude, "seven_day", "seven_day"),
        (SessionProvider::Codex, "seven_day", "10080m"),
        (
            SessionProvider::Codex,
            "seven_day",
            "codex:seven_day:primary",
        ),
        (
            SessionProvider::Codex,
            "codex:seven_day:primary",
            "codex:seven_day:secondary",
        ),
        (
            SessionProvider::Codex,
            "a:seven_day:primary",
            "b:seven_day:primary",
        ),
    ] {
        let mut snapshot = baseline.clone();
        snapshot.provider = provider;
        snapshot.windows[0].window_key = key.into();
        inputs.store.provider_windows = vec![snapshot];
        let mut tracker = Sustain::default();
        let accepted_subject = format!("Codex:{accepted_window}:{}", reset.timestamp());
        tracker.windows.reported_subject(accepted_subject.clone());
        let accepted_spec = HealthAnomalySpec {
            ignore: vec![format!(
                "{KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION}:{accepted_subject}"
            )],
            ..HealthAnomalySpec::default()
        };
        let found = evaluate(&inputs, &thresholds(), &accepted_spec, &mut tracker, now);
        assert_eq!(
            pairs(&found),
            vec![(
                KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
                format!("{provider:?}:{key}:{}", reset.timestamp()),
            )]
        );
    }
    inputs.store.provider_windows = vec![baseline];
    for ignore in [
        vec![],
        vec![format!(
            "{KIND_PROVIDER_AUTH_FAILURE}:Codex:seven_day:{}",
            reset.timestamp()
        )],
        vec![format!(
            "{KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION}:Codex:seven_day"
        )],
        vec![format!(
            "{KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION}:Codex:seven_day:*"
        )],
        vec![format!(
            "{KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION}:Codex:seven_day:{}",
            i64::MIN
        )],
        vec![format!(
            "{KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION}:Codex:seven_day:{}",
            i64::MAX
        )],
    ] {
        let found = evaluate(
            &inputs,
            &thresholds(),
            &HealthAnomalySpec {
                ignore,
                ..HealthAnomalySpec::default()
            },
            &mut Sustain::default(),
            now,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION);
    }
    let other_condition = HealthCondition::new(
        KIND_PROVIDER_AUTH_FAILURE,
        format!("Codex:seven_day:{}", reset.timestamp()),
        "auth failure",
    );
    assert!(!provider_windows::ignored_episode(&spec, &other_condition));
    assert!(!spec.ignores(&other_condition));
    // An unrelated condition remains visible while the window is accepted.
    inputs.host = host_reading(Some(95.0), Some(100.0), None).host;
    assert_eq!(
        pairs(&evaluate(
            &inputs,
            &thresholds(),
            &spec,
            &mut Sustain::default(),
            now
        )),
        vec![(KIND_HUB_DISK_HIGH, "/".into())]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn short_numeric_windows_bound_reset_tolerance_below_a_new_cycle() {
    let reset = DateTime::parse_from_rfc3339("2026-10-26T13:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let now = reset - Duration::seconds(30);
    let mut snapshot = projection_snapshot(now, reset, 0.83);
    snapshot.windows[0].window_key = "codex:1m:secondary".into();
    let mut inputs = Inputs::default();
    inputs.store.provider_windows = vec![snapshot.clone()];
    let subject = format!("Codex:codex:1m:secondary:{}", reset.timestamp());
    let spec = HealthAnomalySpec {
        ignore: vec![format!(
            "{KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION}:{subject}"
        )],
        ..HealthAnomalySpec::default()
    };
    for (delta, expected) in [(-15, 0), (15, 0), (16, 1)] {
        inputs.store.provider_windows[0].windows[0].resets_at_epoch =
            Some(reset.timestamp() + delta);
        let mut tracker = Sustain::default();
        tracker.windows.reported_subject(subject.clone());
        assert_eq!(
            evaluate(&inputs, &thresholds(), &spec, &mut tracker, now).len(),
            expected
        );
    }
    let next_now = now + Duration::minutes(1);
    snapshot.observed_at = next_now;
    snapshot.windows[0].resets_at_epoch = Some(reset.timestamp() + 60);
    inputs.store.provider_windows = vec![snapshot];
    let next = evaluate(
        &inputs,
        &thresholds(),
        &spec,
        &mut Sustain::default(),
        next_now,
    );
    assert_eq!(
        pairs(&next),
        vec![(
            KIND_PROVIDER_WINDOW_PROJECTED_EXHAUSTION,
            format!("Codex:codex:1m:secondary:{}", reset.timestamp() + 60)
        )]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn codex_bucket_and_numeric_lengths_project_like_standard_windows() {
    let now = Utc::now();
    let reset = now + Duration::hours(3);
    for key in [
        "five_hour",
        "300m",
        "codex:five_hour",
        "codex:300m:secondary",
    ] {
        let mut snapshot = projection_snapshot(now, reset, 0.8);
        snapshot.windows[0].window_key = key.into();
        assert_eq!(
            provider_windows::WindowTracker::default()
                .evaluate(&[snapshot], 70, now)
                .len(),
            1,
            "{key}"
        );
    }
}
