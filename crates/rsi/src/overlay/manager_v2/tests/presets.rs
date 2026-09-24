use super::*;
use crate::settings::CustomProviderEntry;
use rsi_common::{
    harness_manager::HarnessManagerScopeModeV1, harness_manager_presets::*, types::SessionProvider,
};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

type Observed = Arc<Mutex<Vec<Value>>>;
/// Accept the form client and independent catalog clients at the actual Unix RPC boundary.
async fn connect_catalog(
    app: &mut App,
    cases: Vec<Case>,
) -> (
    tempfile::TempDir,
    tokio::task::JoinHandle<Vec<Value>>,
    Observed,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let observed: Observed = Arc::default();
    let log = observed.clone();
    let task = tokio::spawn(async move {
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let mut connections = tokio::task::JoinSet::new();
        let mut requests = vec![];
        for (method, mut response) in cases {
            let (request, reply, written) = loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let send = send.clone();
                        connections.spawn(async move {
                            let (read, mut write) = stream.into_split();
                            let mut read = BufReader::new(read);
                            loop {
                                let mut line = String::new();
                                if read.read_line(&mut line).await.unwrap() == 0 { return; }
                                let request: Value = serde_json::from_str(&line).unwrap();
                                let (reply, response) = tokio::sync::oneshot::channel::<Value>();
                                let (written, finished) = tokio::sync::oneshot::channel();
                                if send.send((request, reply, finished)).is_err() { return; }
                                let response = response.await.unwrap();
                                write.write_all(format!("{response}\n").as_bytes()).await.unwrap();
                                let _ = written.send(());
                            }
                        });
                    }
                    request = receive.recv() => break request.unwrap(),
                }
            };
            assert_eq!(request["method"], method);
            response["jsonrpc"] = json!("2.0");
            response["id"] = request["id"].clone();
            log.lock().unwrap().push(request.clone());
            reply.send(response).unwrap();
            written.await.unwrap();
            requests.push(request);
        }
        requests
    });
    app.client = DaemonClient::new(path);
    app.client.connect().await.unwrap();
    (dir, task, observed)
}
fn focus(app: &mut App, field: policy::Field) {
    let state = policy_mut(app);
    state.selected = state.rows().iter().position(|r| r.field == field).unwrap();
}
async fn activate(app: &mut App, field: policy::Field) {
    focus(app, field);
    key(app, KeyCode::Enter).await;
}
async fn catalog_result(app: &mut App) {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        catalog::next_result(&mut app.overlay),
    )
    .await
    .unwrap();
    catalog::dispatch_result(&mut app.overlay, result);
}
fn full(scope: HarnessManagerScopeModeV1) -> ManagerPolicyV2 {
    apply_manager_policy_preset(
        &ManagerPolicyV2::default(),
        ManagerPolicyPreset::FullProjectControl,
        &ManagerPresetContext {
            origin: ManagerPolicyOrigin::New,
            scope_mode: scope,
            touched: &BTreeSet::new(),
        },
    )
    .policy
}

#[tokio::test]
async fn active_session_limit_is_top_level_and_keyboard_save_uses_edited_value() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.row_version = 3;
    saved.policy.max_active_sessions = 20;
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":null})),
            ("ConfigureHarnessManagerPolicy", json!({"result":saved})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();

    assert!(!policy_mut(&mut app).advanced_open);
    let target = policy_mut(&mut app)
        .rows()
        .iter()
        .position(|row| row.field == policy::Field::Concurrency)
        .unwrap();
    assert!(screen(&mut app).contains("Active session limit: 4"));
    for _ in 0..target {
        key(&mut app, KeyCode::Char('j')).await;
    }
    key(&mut app, KeyCode::Enter).await;
    key(&mut app, KeyCode::Backspace).await;
    key(&mut app, KeyCode::Char('2')).await;
    key(&mut app, KeyCode::Char('0')).await;
    key(&mut app, KeyCode::Enter).await;
    key(&mut app, KeyCode::Char('s')).await;

    let requests = finish(task).await;
    assert_eq!(requests[1]["params"]["policy"]["max_active_sessions"], 20);
    assert_eq!(policy_mut(&mut app).draft.max_active_sessions, 20);
    assert!(screen(&mut app).contains("Active session limit: 20"));
}

#[tokio::test]
async fn presets_only_change_draft_and_one_save_adopts_the_actual_reply() {
    let (mut app, config) = fixture();
    app.selected_model = Some("retained-global-selection".into());
    app.model_dropdown = crate::types::ModelDropdownState::new(
        SessionProvider::Local,
        vec![(
            "unrelated-global-choice".into(),
            "Unrelated global picker".into(),
        )],
        None,
    );
    let global = (
        app.selected_provider,
        app.selected_model.clone(),
        app.selected_effort.clone(),
    );
    let mut saved = policy_config(&config);
    saved.policy = full(config.scope_mode);
    saved.row_version = 9;
    let (_dir, task, observed) = connect_catalog(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":null})),
            ("ConfigureHarnessManagerPolicy", json!({"result":saved})),
        ],
    )
    .await;
    policy::open(&mut app, config.clone(), "Coordination desk".into())
        .await
        .unwrap();
    let initial_key = policy_mut(&mut app).idempotency_key.clone();
    assert!(screen(&mut app).contains("Saved: no policy"));
    activate(
        &mut app,
        policy::Field::Preset(ManagerPolicyPreset::Execute),
    )
    .await;
    activate(
        &mut app,
        policy::Field::Preset(ManagerPolicyPreset::Observe),
    )
    .await;
    activate(
        &mut app,
        policy::Field::Preset(ManagerPolicyPreset::FullProjectControl),
    )
    .await;
    activate(&mut app, policy::Field::Preset(ManagerPolicyPreset::Custom)).await;
    assert_eq!(observed.lock().unwrap().len(), 1);
    assert_eq!(
        (
            app.selected_provider,
            app.selected_model.clone(),
            app.selected_effort.clone()
        ),
        global
    );
    assert_eq!(app.model_dropdown.open, false);
    assert_eq!(policy_mut(&mut app).draft, saved.policy);
    assert_ne!(policy_mut(&mut app).idempotency_key, initial_key);
    assert!(screen(&mut app).contains("unsaved changes"));
    assert!(screen(&mut app).contains("Full project control"));
    let request = policy_mut(&mut app).request();
    key(&mut app, KeyCode::Char('s')).await;
    let requests = finish(task).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]["params"],
        serde_json::to_value(request).unwrap()
    );
    assert_eq!(policy_mut(&mut app).draft, saved.policy);
    assert_eq!(policy_mut(&mut app).opened_draft, saved.policy);
    assert_eq!(policy_mut(&mut app).saved.as_ref().unwrap().row_version, 9);
    assert!(screen(&mut app).contains("Saved: Full project control"));
    assert!(screen(&mut app).contains("unchanged"));
}

#[tokio::test]
async fn revoked_saved_custom_roundtrips_all_values_and_explicit_root_override() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.revoked = true;
    saved.policy = full(HarnessManagerScopeModeV1::Project);
    saved.policy.capabilities.reverse();
    saved.policy.max_recovery_attempts = 0;
    saved.policy.max_active_sessions = 12;
    saved.policy.max_created_sessions = 160;
    saved.policy.paused = true;
    saved.policy.paused_epic_ids = config.epic_ids.clone();
    saved.policy.group_ids = vec![Uuid::new_v4()];
    saved.policy.max_spend_usd = Some(17.5);
    saved.policy.retry_delay_seconds = 81;
    saved.policy.request_timeout_seconds = 990;
    saved.policy.provider_limits = vec![
        ManagerProviderLimitV2 {
            provider: SessionProvider::Local,
            max_active: 2,
        },
        ManagerProviderLimitV2 {
            provider: SessionProvider::Codex,
            max_active: 5,
        },
    ];
    saved.policy.allowed_launches = vec![
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Local,
            model: "retained-custom-model".into(),
            effort: Some("retained-level".into())
        };
        2
    ];
    let mut reply = saved.clone();
    reply.row_version += 1;
    reply.revoked = false;
    let (_dir, task, observed) = connect_catalog(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":saved})),
            ("ConfigureHarnessManagerPolicy", json!({"result":reply})),
            ("GetHarnessManagerPolicy", json!({"result":reply})),
        ],
    )
    .await;
    policy::open(&mut app, config.clone(), "Coordination desk".into())
        .await
        .unwrap();
    assert_eq!(policy_mut(&mut app).draft, saved.policy);
    assert!(screen(&mut app).contains("Saved — revoked"));
    let first_key = policy_mut(&mut app).idempotency_key.clone();
    activate(
        &mut app,
        policy::Field::Preset(ManagerPolicyPreset::FullProjectControl),
    )
    .await;
    assert_eq!(policy_mut(&mut app).draft, saved.policy);
    assert_eq!(policy_mut(&mut app).idempotency_key, first_key);
    let state = policy_mut(&mut app);
    assert_eq!(state.classification().preset, ManagerPolicyPreset::Custom);
    assert!(
        state
            .preview_rows()
            .iter()
            .any(|(_, value)| value == "Additional explicit root-Group grant retained")
    );
    state
        .apply_text(policy::Field::ProviderLimit(3), "2")
        .unwrap();
    let mut expected_policy = saved.policy.clone();
    expected_policy
        .provider_limits
        .push(ManagerProviderLimitV2 {
            provider: SessionProvider::OpenRouter,
            max_active: 2,
        });
    assert_eq!(state.draft.provider_limits, expected_policy.provider_limits);
    assert_ne!(state.idempotency_key, first_key);
    assert_eq!(observed.lock().unwrap().len(), 1);
    key(&mut app, KeyCode::Char('s')).await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    let requests = finish(task).await;
    assert_eq!(
        requests[1]["params"]["policy"],
        serde_json::to_value(&expected_policy).unwrap()
    );
    assert_eq!(policy_mut(&mut app).draft, saved.policy);
    assert_eq!(policy_mut(&mut app).saved.as_ref().unwrap().revoked, false);
    assert!(screen(&mut app).contains("retained-custom-model"));
    assert!(screen(&mut app).contains("OPERATOR PAUSE"));
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn revoked_policy_summary_explains_moved_scope_version() {
    let (mut app, mut config) = fixture();
    config.row_version = 23;
    let mut saved = policy_config(&config);
    saved.scope_version = 22;
    saved.revoked = true;
    let (_dir, task) = connect(
        &mut app,
        vec![("GetHarnessManagerPolicy", json!({"result":saved}))],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    assert!(screen(&mut app).contains("scope moved 22 -> 23"));
    finish(task).await;
}

#[tokio::test]
async fn named_zero_suggestions_recheck_stale_fields_and_preserve_other_limits() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.policy = full(config.scope_mode);
    saved.policy.max_active_sessions = 12;
    saved.policy.max_created_containers = 0;
    saved.policy.max_created_sessions = 0;
    saved.policy.max_recovery_attempts = 0;
    saved.policy.max_spend_usd = Some(2.5);
    let mut expected = saved.clone();
    expected.policy.max_created_containers = 8;
    expected.policy.max_created_sessions = 1;
    expected.policy.max_recovery_attempts = 3;
    let (_dir, task, observed) = connect_catalog(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":saved})),
            ("ConfigureHarnessManagerPolicy", json!({"result":expected})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    let stale = policy_mut(&mut app)
        .suggestions()
        .iter()
        .map(|s| s.field)
        .collect::<Vec<_>>();
    policy_mut(&mut app)
        .apply_text(policy::Field::Sessions, "1")
        .unwrap();
    let before = policy_mut(&mut app).request();
    assert!(
        policy_mut(&mut app)
            .apply_suggestions(ManagerPolicyPreset::FullProjectControl, &stale)
            .is_err()
    );
    assert_eq!(policy_mut(&mut app).draft, before.policy);
    assert_eq!(policy_mut(&mut app).idempotency_key, before.idempotency_key);
    activate(&mut app, policy::Field::Suggestions).await;
    assert_eq!(policy_mut(&mut app).draft, expected.policy);
    assert_eq!(observed.lock().unwrap().len(), 1);
    key(&mut app, KeyCode::Char('s')).await;
    assert_eq!(
        finish(task).await[1]["params"]["policy"],
        serde_json::to_value(expected.policy).unwrap()
    );
}

#[tokio::test]
async fn every_builtin_provider_uses_exact_catalog_id_and_default_effort_without_global_changes() {
    for provider in policy::PROVIDERS {
        let (mut app, config) = fixture();
        let mut saved = policy_config(&config);
        saved.policy.allowed_launches = vec![ManagerLaunchChoiceV2 {
            provider,
            model: "retained-unavailable".into(),
            effort: Some("retained-effort".into()),
        }];
        let model = match provider {
            SessionProvider::Claude => "claude-opus-5-5",
            SessionProvider::Codex
            | SessionProvider::CodexAppServer
            | SessionProvider::Pioneer
            | SessionProvider::OpenRouter
            | SessionProvider::Bedrock => "gpt-6-astra",
            _ => "fixture-catalog-id",
        };
        let mut expected = saved.clone();
        expected.policy.allowed_launches[0].model = model.into();
        expected.policy.allowed_launches[0].effort = None;
        let (_dir, task, observed) = connect_catalog(
            &mut app,
            vec![
                ("GetHarnessManagerPolicy", json!({"result":saved})),
                (
                    "DiscoverModels",
                    json!({"result":[[model,"Friendly catalog label"]]}),
                ),
                ("ConfigureHarnessManagerPolicy", json!({"result":expected})),
            ],
        )
        .await;
        policy::open(&mut app, config, "Coordination desk".into())
            .await
            .unwrap();
        let global = (
            app.selected_provider,
            app.selected_model.clone(),
            app.selected_effort.clone(),
            app.available_models.clone(),
        );
        activate(&mut app, policy::Field::EditLaunch(0)).await;
        catalog_result(&mut app).await;
        assert!(screen(&mut app).contains("retained-unavailable"));
        assert!(screen(&mut app).contains("Friendly catalog label"));
        key(&mut app, KeyCode::Enter).await;
        assert!(screen(&mut app).contains("Default (no explicit effort)"));
        key(&mut app, KeyCode::Enter).await;
        assert_eq!(policy_mut(&mut app).draft, expected.policy);
        assert_eq!(
            (
                app.selected_provider,
                app.selected_model.clone(),
                app.selected_effort.clone(),
                app.available_models.clone()
            ),
            global
        );
        assert_eq!(observed.lock().unwrap().len(), 2);
        key(&mut app, KeyCode::Char('s')).await;
        let requests = finish(task).await;
        assert_eq!(
            requests[1]["params"]["provider"],
            serde_json::to_value(provider).unwrap()
        );
        assert_eq!(
            requests[2]["params"]["policy"],
            serde_json::to_value(expected.policy).unwrap()
        );
    }
}

#[tokio::test]
async fn failed_empty_and_stale_catalogs_keep_exact_draft_and_retry_key_then_allow_save() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.policy.allowed_launches = vec![ManagerLaunchChoiceV2 {
        provider: SessionProvider::Local,
        model: "retained-legacy-model".into(),
        effort: Some("old-effort".into()),
    }];
    let (_dir, task, _) = connect_catalog(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":saved})),
            (
                "DiscoverModels",
                json!({"error":{"code":-32000,"message":"catalog offline"}}),
            ),
            ("DiscoverModels", json!({"result":[]})),
            ("ConfigureHarnessManagerPolicy", json!({"result":saved})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    let original = policy_mut(&mut app).request();
    activate(&mut app, policy::Field::EditLaunch(0)).await;
    catalog_result(&mut app).await;
    assert!(screen(&mut app).contains("catalog offline"));
    assert!(screen(&mut app).contains("retained-legacy-model"));
    let old = policy_mut(&mut app)
        .picker
        .as_ref()
        .unwrap()
        .request_generation;
    key(&mut app, KeyCode::Char('r')).await;
    let state = policy_mut(&mut app);
    state.apply_catalog_result(catalog::ManagerCatalogResult {
        generation: old,
        provider: SessionProvider::Local,
        outcome: Ok(vec![("stale-choice".into(), "Stale".into())]),
    });
    assert!(state.catalog_request.is_some());
    assert_eq!(
        state.picker.as_ref().unwrap().status,
        catalog::CatalogStatus::Loading
    );
    let current = state.picker.as_ref().unwrap().request_generation;
    state.apply_catalog_result(catalog::ManagerCatalogResult {
        generation: current,
        provider: SessionProvider::Claude,
        outcome: Ok(vec![]),
    });
    assert!(state.catalog_request.is_some());
    catalog_result(&mut app).await;
    assert!(screen(&mut app).contains("no selectable models"));
    key(&mut app, KeyCode::Enter).await;
    key(&mut app, KeyCode::Esc).await;
    assert_eq!(policy_mut(&mut app).draft, original.policy);
    assert_eq!(
        policy_mut(&mut app).idempotency_key,
        original.idempotency_key
    );
    key(&mut app, KeyCode::Char('s')).await;
    assert_eq!(
        finish(task).await[3]["params"],
        serde_json::to_value(original).unwrap()
    );
}

#[test]
fn provider_wrapper_and_exact_effort_ladders_cover_appserver_and_unknown_capabilities() {
    use catalog::{ManagerEffortChoices, manager_launch_effort_choices};
    let mut picker = catalog::ManagerLaunchPickerState::new(None, None);
    for provider in policy::PROVIDERS {
        assert_eq!(picker.model_dropdown.provider, provider);
        picker.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    }
    assert_eq!(picker.model_dropdown.provider, policy::PROVIDERS[0]);
    for provider in [SessionProvider::Codex, SessionProvider::CodexAppServer] {
        let menu = manager_launch_effort_choices(provider, "gpt-6-astra");
        assert!(menu.values().contains(&Some("ultra".into())));
        assert_eq!(menu.values()[0], None);
        assert_eq!(
            manager_launch_effort_choices(provider, "future-reasoning-model"),
            ManagerEffortChoices::DefaultOnly {
                capability_unknown: true
            }
        );
    }
    assert_eq!(
        manager_launch_effort_choices(SessionProvider::Claude, "claude-opus-4-5").values(),
        vec![
            None,
            Some("low".into()),
            Some("medium".into()),
            Some("high".into())
        ]
    );
    assert!(
        manager_launch_effort_choices(SessionProvider::Pioneer, "claude-opus-5")
            .values()
            .contains(&Some("xhigh".into()))
    );
    for provider in [
        SessionProvider::Local,
        SessionProvider::Harness,
        SessionProvider::Antigravity,
    ] {
        assert_eq!(
            manager_launch_effort_choices(provider, "fixture-model").values(),
            vec![None]
        );
    }
}

#[tokio::test]
async fn custom_endpoints_are_named_without_inventing_local_choices_and_cancel_is_read_only() {
    let (mut app, config) = fixture();
    app.settings.custom_providers.push(CustomProviderEntry {
        id: Uuid::new_v4(),
        name: "Named research endpoint".into(),
        base_url: "http://unused.invalid".into(),
        api_key: "fixture-key".into(),
        default_model: "custom-default".into(),
    });
    let (_dir, task) = connect(
        &mut app,
        vec![(
            "GetHarnessManagerPolicy",
            json!({"result":policy_config(&config)}),
        )],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    let original = policy_mut(&mut app).request();
    let state = policy_mut(&mut app);
    assert!(
        state
            .preview_rows()
            .iter()
            .any(|(label, value)| label.contains("Named research endpoint")
                && value.contains("Endpoint-specific manager restrictions unsupported"))
    );
    state.activate(policy::Field::AddLaunch);
    assert_eq!(
        state
            .picker
            .as_ref()
            .unwrap()
            .model_dropdown
            .custom_provider_index,
        None
    );
    key(&mut app, KeyCode::Esc).await;
    assert_eq!(policy_mut(&mut app).draft, original.policy);
    assert_eq!(
        policy_mut(&mut app).idempotency_key,
        original.idempotency_key
    );
    assert_eq!(policy_mut(&mut app).draft.allowed_launches, vec![]);
    assert_eq!(finish(task).await.len(), 1);
}

#[tokio::test]
async fn keyboard_identical_zero_is_touched_and_cancelling_text_keeps_the_key() {
    let (mut app, config) = fixture();
    let (_dir, task) = connect(
        &mut app,
        vec![("GetHarnessManagerPolicy", json!({"result":null}))],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    let original_key = policy_mut(&mut app).idempotency_key.clone();
    activate(&mut app, policy::Field::Preset(ManagerPolicyPreset::Custom)).await;
    activate(&mut app, policy::Field::Retries).await;
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(policy_mut(&mut app).idempotency_key, original_key);
    assert!(
        policy_mut(&mut app)
            .touched
            .contains(&ManagerPolicyField::RecoveryAttempts)
    );
    activate(&mut app, policy::Field::Concurrency).await;
    key(&mut app, KeyCode::Char('2')).await;
    key(&mut app, KeyCode::Esc).await;
    assert_eq!(policy_mut(&mut app).draft.max_active_sessions, 4);
    assert_eq!(policy_mut(&mut app).idempotency_key, original_key);
    activate(
        &mut app,
        policy::Field::Preset(ManagerPolicyPreset::Execute),
    )
    .await;
    assert_eq!(policy_mut(&mut app).draft.max_recovery_attempts, 0);
    assert_eq!(policy_mut(&mut app).draft.max_created_sessions, 32);
    assert_eq!(
        policy_mut(&mut app).classification().preset,
        ManagerPolicyPreset::Custom
    );
    assert!(screen(&mut app).contains("Use suggested allowances"));
    assert_eq!(finish(task).await.len(), 1);
}

#[tokio::test]
async fn identical_stale_retry_preserves_request_and_explicit_reload_reads_new_fences() {
    let (mut app, config) = fixture();
    let mut fresh_config = config.clone();
    fresh_config.row_version = 4;
    let mut fresh_policy = policy_config(&fresh_config);
    fresh_policy.row_version = 8;
    fresh_policy.scope_version = 4;
    fresh_policy.policy.max_active_sessions = 9;
    let (_dir, task, _) = connect_catalog(
        &mut app,
        vec![
            (
                "GetHarnessManagerPolicy",
                json!({"result":policy_config(&config)}),
            ),
            (
                "ConfigureHarnessManagerPolicy",
                json!({"error":{"code":-32000,"message":"stale policy version"}}),
            ),
            (
                "ConfigureHarnessManagerPolicy",
                json!({"error":{"code":-32000,"message":"stale policy version"}}),
            ),
            ("GetHarnessManager", json!({"result":fresh_config})),
            ("GetHarnessManagerPolicy", json!({"result":fresh_policy})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    activate(
        &mut app,
        policy::Field::Preset(ManagerPolicyPreset::Observe),
    )
    .await;
    let request = policy_mut(&mut app).request();
    key(&mut app, KeyCode::Char('s')).await;
    key(&mut app, KeyCode::Char('s')).await;
    assert_eq!(
        policy_mut(&mut app).idempotency_key,
        request.idempotency_key
    );
    assert_eq!(policy_mut(&mut app).draft, request.policy);
    policy_mut(&mut app)
        .apply_text(policy::Field::Concurrency, "12")
        .unwrap();
    assert_ne!(
        policy_mut(&mut app).idempotency_key,
        request.idempotency_key
    );
    key(&mut app, KeyCode::Char('r')).await;
    let requests = finish(task).await;
    assert_eq!(requests[1]["params"], requests[2]["params"]);
    assert_eq!(
        requests[1]["params"],
        serde_json::to_value(request).unwrap()
    );
    assert_eq!(policy_mut(&mut app).draft, fresh_policy.policy);
    assert!(screen(&mut app).contains("scope 4 / policy 8"));
    assert_eq!(policy_mut(&mut app).request().expected_scope_version, 4);
    assert_eq!(policy_mut(&mut app).request().expected_policy_version, 8);
    let state = policy_mut(&mut app);
    assert_eq!(state.draft, state.opened_draft);
}

#[tokio::test]
async fn catalog_selection_saves_explicit_supported_effort_as_an_exact_tuple() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.policy.allowed_launches = vec![ManagerLaunchChoiceV2 {
        provider: SessionProvider::CodexAppServer,
        model: "retained-old".into(),
        effort: None,
    }];
    let mut expected = saved.clone();
    expected.policy.allowed_launches = vec![ManagerLaunchChoiceV2 {
        provider: SessionProvider::CodexAppServer,
        model: "gpt-6-astra".into(),
        effort: Some("medium".into()),
    }];
    let (_dir, task, _) = connect_catalog(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":saved})),
            (
                "DiscoverModels",
                json!({"result":[["gpt-6-astra","Terra catalog label"]]}),
            ),
            ("ConfigureHarnessManagerPolicy", json!({"result":expected})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    activate(&mut app, policy::Field::EditLaunch(0)).await;
    catalog_result(&mut app).await;
    key(&mut app, KeyCode::Enter).await;
    key(&mut app, KeyCode::Char('j')).await;
    key(&mut app, KeyCode::Char('j')).await;
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(policy_mut(&mut app).draft, expected.policy);
    key(&mut app, KeyCode::Char('s')).await;
    assert_eq!(
        finish(task).await[2]["params"]["policy"],
        serde_json::to_value(expected.policy).unwrap()
    );
}

#[tokio::test]
async fn duplicate_catalog_choice_keeps_existing_rows_and_32_choice_limit_blocks_append() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.policy.allowed_launches = vec![
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-opus-5".into(),
            effort: None
        };
        2
    ];
    let (_dir, task, _) = connect_catalog(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":saved})),
            (
                "DiscoverModels",
                json!({"result":[["claude-opus-5","Opus catalog"]]}),
            ),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    let original = policy_mut(&mut app).request();
    activate(&mut app, policy::Field::AddLaunch).await;
    catalog_result(&mut app).await;
    key(&mut app, KeyCode::Enter).await;
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(policy_mut(&mut app).draft, original.policy);
    assert_eq!(
        policy_mut(&mut app).idempotency_key,
        original.idempotency_key
    );
    assert!(screen(&mut app).contains("Already listed"));
    let state = policy_mut(&mut app);
    state
        .draft
        .allowed_launches
        .resize(32, original.policy.allowed_launches[0].clone());
    let before = state.request();
    state.activate(policy::Field::AddLaunch);
    assert!(state.picker.is_none());
    assert_eq!(state.draft, before.policy);
    assert_eq!(state.idempotency_key, before.idempotency_key);
    assert_eq!(
        state.error.as_deref(),
        Some("At most 32 allowed launch choices.")
    );
    assert_eq!(finish(task).await.len(), 2);
}

#[test]
fn catalog_scroll_keeps_last_model_identity_visible_in_small_viewport() {
    let mut state = crate::types::ModelDropdownState::new(
        SessionProvider::CodexAppServer,
        (0..70000)
            .map(|i| (format!("catalog-id-{i}"), format!("Catalog model {i}")))
            .collect(),
        None,
    );
    state.selected_index = 69999;
    let mut terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::widget::model_dropdown::render_model_dropdown(
                frame,
                frame.area(),
                ratatui::layout::Rect::new(1, 2, 50, 1),
                &state,
                None,
                true,
            )
        })
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(rendered.contains("Catalog model 69999"));
    assert!(rendered.contains("catalog-id-69999"));
    assert!(rendered.contains("Codex(AS)"));
}

#[tokio::test]
async fn cancel_inflight_catalog_aborts_its_reader_and_late_result_cannot_edit_policy() {
    let (mut app, config) = fixture();
    let saved = policy_config(&config);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("held.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (seen_send, seen) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        let get: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(get["method"], "GetHarnessManagerPolicy");
        write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0", "id":get["id"], "result":saved})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write_catalog) = stream.into_split();
        let mut read_catalog = BufReader::new(read);
        line.clear();
        read_catalog.read_line(&mut line).await.unwrap();
        let discovery: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(discovery["method"], "DiscoverModels");
        seen_send.send(()).unwrap();
        held.await.unwrap();
        // Cancellation closes this client; either a buffered write or BrokenPipe is legal.
        let _ = write_catalog.write_all(format!("{}\n", json!({"jsonrpc":"2.0", "id":discovery["id"], "result":[["late-model","Late catalog"]]})).as_bytes()).await;
        vec![get, discovery]
    });
    app.client = DaemonClient::new(path);
    app.client.connect().await.unwrap();
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    let original = policy_mut(&mut app).request();
    activate(&mut app, policy::Field::AddLaunch).await;
    tokio::time::timeout(Duration::from_secs(3), seen)
        .await
        .unwrap()
        .unwrap();
    let generation = policy_mut(&mut app)
        .picker
        .as_ref()
        .unwrap()
        .request_generation;
    key(&mut app, KeyCode::Esc).await;
    let state = policy_mut(&mut app);
    assert!(state.picker.is_none());
    assert!(state.catalog_request.is_none());
    state.apply_catalog_result(catalog::ManagerCatalogResult {
        generation,
        provider: SessionProvider::Claude,
        outcome: Ok(vec![("late-model".into(), "Late catalog".into())]),
    });
    assert_eq!(state.draft, original.policy);
    assert_eq!(state.idempotency_key, original.idempotency_key);
    release.send(()).unwrap();
    assert_eq!(finish(task).await.len(), 2);
}

/// K14 (#672) design test 9: the Operator delegation grant is a visible,
/// toggleable Authority row, and Full project control saves it.
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn operator_delegation_row_renders_and_saving_full_stores_the_grant() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.policy = full(config.scope_mode);
    saved.row_version = 5;
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":null})),
            ("ConfigureHarnessManagerPolicy", json!({"result":saved})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    let field = policy::Field::Capability(10);
    let row = policy_mut(&mut app)
        .rows()
        .into_iter()
        .find(|r| r.field == field)
        .unwrap();
    assert_eq!(row.label, "Grant Operator delegation");
    assert_eq!(row.value, "false");
    activate(
        &mut app,
        policy::Field::Preset(ManagerPolicyPreset::FullProjectControl),
    )
    .await;
    focus(&mut app, field);
    let text = screen(&mut app);
    assert!(text.contains("Grant Operator delegation: true"), "{text}");
    assert!(text.contains("Call allowlisted operator methods"), "{text}");
    assert!(
        policy_mut(&mut app)
            .draft
            .capabilities
            .contains(&ManagerCapabilityV2::OperatorDelegation)
    );
    key(&mut app, KeyCode::Char('s')).await;
    let requests = finish(task).await;
    let grants = requests[1]["params"]["policy"]["capabilities"]
        .as_array()
        .unwrap()
        .clone();
    assert!(grants.contains(&json!("operator_delegation")), "{grants:?}");
    let state = policy_mut(&mut app);
    assert!(
        state.notice.contains("OperatorDelegation"),
        "{}",
        state.notice
    );
    assert!(
        state
            .notice
            .starts_with("Policy saved as Full project control"),
        "{}",
        state.notice
    );
    assert!(screen(&mut app).contains("Saved: Full project control"));
}

/// A Full policy saved before K14 keeps its exact grants: it reads as Custom
/// and the new grant row shows its saved `false` until the operator acts.
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn older_saved_full_without_operator_delegation_renders_custom_unwidened() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.policy = full(config.scope_mode);
    saved
        .policy
        .capabilities
        .retain(|c| *c != ManagerCapabilityV2::OperatorDelegation);
    let older = saved.policy.clone();
    let (_dir, task) = connect(
        &mut app,
        vec![("GetHarnessManagerPolicy", json!({"result":saved}))],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    finish(task).await;
    let state = policy_mut(&mut app);
    assert_eq!(state.draft, older);
    assert_eq!(state.opened_draft, older);
    assert_eq!(state.classification().preset, ManagerPolicyPreset::Custom);
    assert!(state.saved_summary().starts_with("Saved: Custom"));
    let row = state
        .rows()
        .into_iter()
        .find(|r| r.field == policy::Field::Capability(10))
        .unwrap();
    assert_eq!(row.value, "false");
    assert!(!row.modified);
    let text = screen(&mut app);
    assert!(text.contains("Saved: Custom"), "{text}");
    assert!(text.contains("Draft: Custom"), "{text}");
}

/// Toggling the grant on a Custom draft marks it modified against the saved
/// value and one save stores it alongside the retained grants.
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn toggling_operator_delegation_on_custom_draft_saves_it() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.policy.mode = ManagerOperatingModeV2::Execute;
    saved.policy.capabilities = vec![ManagerCapabilityV2::WorkPlan];
    let mut reply = saved.clone();
    reply.row_version += 1;
    reply
        .policy
        .capabilities
        .push(ManagerCapabilityV2::OperatorDelegation);
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":saved})),
            ("ConfigureHarnessManagerPolicy", json!({"result":reply})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    assert_eq!(
        policy_mut(&mut app).classification().preset,
        ManagerPolicyPreset::Custom
    );
    let field = policy::Field::Capability(10);
    activate(&mut app, field).await;
    let row = policy_mut(&mut app)
        .rows()
        .into_iter()
        .find(|r| r.field == field)
        .unwrap();
    assert_eq!(row.value, "true");
    assert!(row.modified);
    assert_eq!(row.saved.as_deref(), Some("false"));
    let text = screen(&mut app);
    assert!(
        text.contains("Grant Operator delegation: true (saved false)"),
        "{text}"
    );
    key(&mut app, KeyCode::Char('s')).await;
    let requests = finish(task).await;
    assert_eq!(
        requests[1]["params"]["policy"]["capabilities"],
        json!(["work_plan", "operator_delegation"])
    );
    let state = policy_mut(&mut app);
    assert_eq!(state.draft, reply.policy);
    assert!(
        state.notice.contains("grants WorkPlan, OperatorDelegation"),
        "{}",
        state.notice
    );
}
