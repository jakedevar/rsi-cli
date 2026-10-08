use super::*;
mod presets;
use crate::{
    app::app_test_helpers::{baseline_session, with_focused_kind},
    client::DaemonClient,
    types::SessionState,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use rsi_common::{harness_manager_v2::*, types::SessionKind};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

fn fixture() -> (App, HarnessManagerConfigV1) {
    let mut app = with_focused_kind(SessionKind::Standard, None);
    let id = app.selected_session_id().unwrap();
    let project = Uuid::new_v4();
    let session = &mut app.sessions.get_mut(&id).unwrap().session;
    session.project_id = Some(project);
    session.title = Some("Coordination desk".into());
    let mut epic = baseline_session(Uuid::new_v4(), SessionKind::Epic);
    epic.title = Some("Orchestration Agent Process Fix".into());
    epic.project_id = Some(project);
    let config = HarnessManagerConfigV1 {
        scope_mode: rsi_common::harness_manager::HarnessManagerScopeModeV1::Selected,
        selected_epic_ids: None,
        group_ids: Vec::new(),
        project_id: project,
        manager_session_id: id,
        current_session_id: Some(id),
        epic_ids: vec![epic.id],
        row_version: 3,
        updated_at: chrono::Utc::now(),
    };
    app.sessions.insert(epic.id, SessionState::new(epic));
    (app, config)
}
fn policy_config(config: &HarnessManagerConfigV1) -> HarnessManagerPolicyConfigV2 {
    HarnessManagerPolicyConfigV2 {
        project_id: config.project_id,
        manager_session_id: config.manager_session_id,
        scope_version: 3,
        row_version: 2,
        policy: ManagerPolicyV2::default(),
        updated_at: chrono::Utc::now(),
        revoked: false,
    }
}
fn page(
    config: &HarnessManagerConfigV1,
    section: ManagerInspectSectionV2,
    rows: Vec<Value>,
    next: Option<&str>,
) -> ManagerInspectionV2 {
    ManagerInspectionV2 {
        observed_at: chrono::Utc::now(),
        scope_version: 3,
        policy: Some(policy_config(config)),
        section,
        rows,
        next_cursor: next.map(str::to_string),
        complete: next.is_none(),
    }
}
type Case = (&'static str, Value);
/// The composite Board's bounded load: Overview, Decisions, Requests, Health
/// first pages.
fn board_cases(
    config: &HarnessManagerConfigV1,
    overview: Vec<Value>,
    decisions: Vec<Value>,
    requests: Vec<Value>,
) -> Vec<Case> {
    [
        (ManagerInspectSectionV2::Overview, overview),
        (ManagerInspectSectionV2::Decisions, decisions),
        (ManagerInspectSectionV2::Requests, requests),
        (ManagerInspectSectionV2::Health, vec![]),
    ]
    .into_iter()
    .map(|(section, rows)| {
        (
            "GetHarnessManagerState",
            json!({"result":page(config, section, rows, None)}),
        )
    })
    .collect()
}
async fn connect(
    app: &mut App,
    cases: Vec<Case>,
) -> (tempfile::TempDir, tokio::task::JoinHandle<Vec<Value>>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut requests = vec![];
        for (method, mut response) in cases {
            let mut line = String::new();
            assert!(read.read_line(&mut line).await.unwrap() > 0, "{method}");
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], method);
            response["jsonrpc"] = json!("2.0");
            response["id"] = request["id"].clone();
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            requests.push(request);
        }
        requests
    });
    app.client = DaemonClient::new(path);
    app.client.connect().await.unwrap();
    (dir, task)
}
async fn finish(task: tokio::task::JoinHandle<Vec<Value>>) -> Vec<Value> {
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
}
async fn key(app: &mut App, key: KeyCode) {
    assert!(crate::overlay::handle_overlay_key(app, KeyEvent::new(key, KeyModifiers::NONE)).await);
}
fn policy_mut(app: &mut App) -> &mut policy::PolicyState {
    super::policy_mut(app).unwrap()
}
fn board_state(app: &App) -> &board::BoardState {
    match &app.overlay {
        OverlayState::HarnessManagerV2(view) => &view.ledger,
        _ => panic!(),
    }
}
fn screen(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), app))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}

#[tokio::test]
async fn operator_policy_edits_every_field_and_saves_current_fences() {
    use policy::Field::*;
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.row_version = 3;
    let (_dir, task) = connect(
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
    let epic = config.epic_ids[0];
    let group = Uuid::new_v4();
    let expected = {
        let s = policy_mut(&mut app);
        s.advanced_open = true;
        s.activate(Mode);
        s.activate(Mode);
        s.activate(Paused);
        for i in 0..7 {
            s.activate(Capability(i));
        }
        s.activate(Epic(epic));
        s.activate(CreateGroups);
        s.activate(Tier2Reviewer);
        for (field, value) in [
            (Containers, "8"),
            (Sessions, "16"),
            (Concurrency, "3"),
            (Retries, "2"),
            (RetryDelay, "120"),
            (Deadline, "1800"),
            (Spend, "12.50"),
            (ProviderLimit(1), "2"),
            (SettingBound(0), "4096-32768"),
        ] {
            s.apply_text(field, value).unwrap();
        }
        s.apply_text(AddGroup, &group.to_string()).unwrap();
        let derived_epic = Uuid::new_v4();
        s.apply_text(AddPausedEpic, &derived_epic.to_string())
            .unwrap();
        assert!(s.rows().iter().any(|row| row.field == Epic(derived_epic)));
        s.activate(Epic(derived_epic));
        s.advanced_open = true;
        s.activate(AddRawLaunch);
        s.edit = None;
        s.activate(LaunchProvider(0));
        s.apply_text(LaunchModel(0), "configured-model").unwrap();
        s.apply_text(LaunchEffort(0), "high").unwrap();
        assert!(s.draft.validate().is_ok());
        let serialized = serde_json::to_value(&s.draft).unwrap();
        let defaults = serde_json::to_value(ManagerPolicyV2::default()).unwrap();
        assert_eq!(
            serialized.as_object().unwrap().len(),
            defaults.as_object().unwrap().len()
        );
        for (field, value) in serialized.as_object().unwrap() {
            assert_ne!(
                value, &defaults[field],
                "field {field} has no exercised form edit"
            );
        }
        s.request()
    };
    key(&mut app, KeyCode::Char('s')).await;
    let requests = finish(task).await;
    assert_eq!(
        requests[1]["params"],
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(policy_mut(&mut app).policy_version, 3);
    assert!(screen(&mut app).contains("Coordination desk"));
}

/// #633: the operator grants the topology `Automation` capability from the
/// policy editor (no raw SQL), and the saved policy carries its wire name.
#[tokio::test]
async fn operator_policy_grants_topology_automation_capability() {
    let (mut app, config) = fixture();
    let mut saved = policy_config(&config);
    saved.row_version = 3;
    saved.policy.capabilities = vec![ManagerCapabilityV2::Automation];
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
    {
        let s = policy_mut(&mut app);
        s.advanced_open = true;
        let row = s
            .rows()
            .into_iter()
            .find(|row| row.label == "Grant Automation")
            .expect("Automation capability row");
        assert_eq!(
            row.description,
            "Author, run, interrupt and resolve topologies on scoped Epics"
        );
        s.activate(row.field);
        assert!(
            s.draft
                .capabilities
                .contains(&ManagerCapabilityV2::Automation)
        );
    }
    key(&mut app, KeyCode::Char('s')).await;
    let requests = finish(task).await;
    assert_eq!(
        requests[1]["params"]["policy"]["capabilities"],
        json!(["automation"])
    );
}

#[tokio::test]
async fn policy_empty_launch_list_shows_intent_hold_and_saves_after_last_restriction_removed() {
    use policy::Field::*;
    let (mut app, config) = fixture();
    let mut restricted = policy_config(&config);
    restricted.row_version = 3;
    restricted
        .policy
        .allowed_launches
        .push(ManagerLaunchChoiceV2 {
            provider: policy::PROVIDERS[0],
            model: "configured-model".into(),
            effort: Some("high".into()),
        });
    let mut unrestricted = policy_config(&config);
    unrestricted.row_version = 4;
    let (_dir, task) = connect(
        &mut app,
        vec![
            (
                "GetHarnessManagerPolicy",
                json!({"result":policy_config(&config)}),
            ),
            (
                "ConfigureHarnessManagerPolicy",
                json!({"result":restricted}),
            ),
            (
                "ConfigureHarnessManagerPolicy",
                json!({"result":unrestricted}),
            ),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    {
        let s = policy_mut(&mut app);
        s.selected = s.rows().iter().position(|r| r.field == AddLaunch).unwrap();
    }
    assert!(screen(&mut app).contains("automatic recovery: held"));
    {
        let s = policy_mut(&mut app);
        s.advanced_open = true;
        s.activate(AddRawLaunch);
        s.apply_text(LaunchModel(0), "configured-model").unwrap();
        s.apply_text(LaunchEffort(0), "high").unwrap();
        s.edit = None;
        s.selected = s.rows().iter().position(|r| r.field == AddLaunch).unwrap();
    }
    assert!(screen(&mut app).contains("Restricted to 1 exact choices"));
    assert!(screen(&mut app).contains("configured-model"));
    key(&mut app, KeyCode::Char('s')).await;
    policy_mut(&mut app).activate(RemoveLaunch(0));
    assert!(screen(&mut app).contains("automatic recovery: held"));
    key(&mut app, KeyCode::Char('s')).await;
    let requests = finish(task).await;
    assert_eq!(
        requests[1]["params"]["policy"]["allowed_launches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        requests[2]["params"]["policy"]["allowed_launches"],
        json!([])
    );
    assert_eq!(policy_mut(&mut app).policy_version, 4);
    assert!(policy_mut(&mut app).draft.allowed_launches.is_empty());
    assert!(screen(&mut app).contains("automatic recovery: held"));
}

#[tokio::test]
async fn policy_stale_error_keeps_draft_and_retry_key_and_space_is_owned() {
    let (mut app, config) = fixture();
    let (_dir, task) = connect(
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
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    key(&mut app, KeyCode::Char(' ')).await;
    assert!(!app.overlay_leader_pending);
    let before = policy_mut(&mut app).request();
    key(&mut app, KeyCode::Char('s')).await;
    finish(task).await;
    assert_eq!(
        policy_mut(&mut app).draft.mode,
        ManagerOperatingModeV2::Monitor
    );
    assert_eq!(policy_mut(&mut app).idempotency_key, before.idempotency_key);
    assert!(screen(&mut app).contains("stale policy version"));
    assert!(screen(&mut app).contains("Coordination desk"));
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_empty_filtered_page_can_advance_and_go_back_without_inventing_completion() {
    let (mut app, config) = fixture();
    let first = page(
        &config,
        ManagerInspectSectionV2::Work,
        vec![],
        Some("scope-fenced-page"),
    );
    let second = page(
        &config,
        ManagerInspectSectionV2::Work,
        vec![
            json!({"type":"work","title":"Orchestration Agent Process Fix","partial_count":2,"accepted_count":1,"integrated_count":0,"total_count":17,"evidence":null}),
        ],
        None,
    );
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerState", json!({"result":first})),
            ("GetHarnessManagerState", json!({"result":second})),
            ("GetHarnessManagerState", json!({"result":first})),
        ],
    )
    .await;
    board::open_section(
        &mut app,
        config,
        "Coordination desk".into(),
        ManagerSection::Inspect(ManagerInspectSectionV2::Work),
    )
    .await
    .unwrap();
    assert!(screen(&mut app).contains("more pages"));
    key(&mut app, KeyCode::Char('n')).await;
    let text = screen(&mut app);
    for value in [
        "Coordination desk",
        "Orchestration Agent Process Fix",
        "partial count: 2",
        "accepted count: 1",
        "integrated count: 0",
        "total count: 17",
        "evidence: unknown",
    ] {
        assert!(text.contains(value), "{value}");
    }
    key(&mut app, KeyCode::Char('p')).await;
    let requests = finish(task).await;
    assert_eq!(
        requests[1]["params"]["query"]["cursor"],
        "scope-fenced-page"
    );
    assert_eq!(requests[2]["params"]["query"]["cursor"], Value::Null);
    assert_eq!(board_state(&app).inspection.rows.len(), 0);
}

#[tokio::test]
async fn decision_answer_uses_exact_digest_version_and_stale_error_keeps_pending_target() {
    let (mut app, config) = fixture();
    let decision = json!({"type":"decision","key":"release-target","row_version":7,"payload":{"status":"pending","question":format!("Choose release target {}", "long question ".repeat(40)),"target_digest":"sha256:exact-target","epic_id":config.epic_ids[0]}});
    let inspection = page(
        &config,
        ManagerInspectSectionV2::Decisions,
        vec![decision],
        None,
    );
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerState", json!({"result":inspection})),
            (
                "AnswerHarnessManagerDecision",
                json!({"error":{"code":-32000,"message":"stale decision target"}}),
            ),
        ],
    )
    .await;
    board::open(&mut app, config.clone(), "Coordination desk".into(), true)
        .await
        .unwrap();
    key(&mut app, KeyCode::Enter).await;
    for c in "rolling".chars() {
        key(&mut app, KeyCode::Char(c)).await;
    }
    let expected = board_state(&app).answer.as_ref().unwrap().target.clone();
    key(&mut app, KeyCode::Enter).await;
    let requests = finish(task).await;
    assert_eq!(
        requests[1]["params"],
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(expected.fence.scope_version, 3);
    assert_eq!(expected.fence.policy_version, 2);
    assert_eq!(expected.expected_row_version, 7);
    assert_eq!(expected.target_digest, "sha256:exact-target");
    assert_eq!(expected.decision_key, "release-target");
    assert_eq!(
        board_state(&app).answer.as_ref().unwrap().target.answer,
        "rolling"
    );
    assert_eq!(
        board::row_payload(&board_state(&app).inspection.rows[0])["status"],
        "pending"
    );
    assert!(screen(&mut app).contains("stale decision target"));
    assert!(screen(&mut app).contains("Choose release target"));
}

#[tokio::test]
async fn board_tab_navigation_enters_decisions_and_missing_target_is_refused() {
    let (mut app, config) = fixture();
    let decisions = page(&config, ManagerInspectSectionV2::Decisions, vec![], None);
    let mut cases = board_cases(&config, vec![], vec![], vec![]);
    cases.push(("GetHarnessManagerState", json!({"result":decisions})));
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    key(&mut app, KeyCode::Tab).await;
    let requests = finish(task).await;
    assert_eq!(requests[4]["params"]["query"]["section"], "decisions");
    assert!(screen(&mut app).contains("Decisions"));
    assert!(board_state(&app).decision_draft().is_err());
}

#[tokio::test]
async fn unified_surface_reaches_all_sections_from_one_entry() {
    let (mut app, config) = fixture();
    let mut cases = board_cases(&config, vec![], vec![], vec![]);
    cases.extend([
        (
            "GetHarnessManagerPolicy",
            json!({"result":policy_config(&config)}),
        ),
        (
            "GetHarnessManagerState",
            json!({"result":page(&config, ManagerInspectSectionV2::Decisions, vec![], None)}),
        ),
        (
            "GetHarnessManagerState",
            json!({"result":page(&config, ManagerInspectSectionV2::Requests, vec![], None)}),
        ),
        (
            "GetHarnessManagerState",
            json!({"result":page(&config, ManagerInspectSectionV2::Workers, vec![], None)}),
        ),
        (
            "GetHarnessManagerState",
            json!({"result":page(&config, ManagerInspectSectionV2::Work, vec![], None)}),
        ),
    ]);
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    key(&mut app, KeyCode::Char('5')).await;
    assert!(screen(&mut app).contains("Active session limit"));
    key(&mut app, KeyCode::Char('2')).await;
    key(&mut app, KeyCode::Char('3')).await;
    assert!(screen(&mut app).contains("[3 Inbox]"));
    key(&mut app, KeyCode::Char('4')).await;
    key(&mut app, KeyCode::Char(']')).await;
    let requests = finish(task).await;
    let fetched = requests
        .iter()
        .filter_map(|r| r["params"]["query"]["section"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        fetched,
        [
            "overview",
            "decisions",
            "requests",
            "health",
            "decisions",
            "requests",
            "workers",
            "work"
        ]
    );
    assert!(screen(&mut app).contains("Work"));
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "GetHarnessManagerPolicy")
            .count(),
        1
    );
}

#[tokio::test]
async fn policy_draft_survives_section_switch_and_saves_edited_value() {
    use policy::Field::Concurrency;
    let (mut app, config) = fixture();
    let saved = policy_config(&config);
    let mut cases = board_cases(&config, vec![], vec![], vec![]);
    cases.push((
        "GetHarnessManagerPolicy",
        json!({"result":policy_config(&config)}),
    ));
    cases.extend(board_cases(&config, vec![], vec![], vec![]));
    cases.push(("ConfigureHarnessManagerPolicy", json!({"result":saved})));
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    key(&mut app, KeyCode::Char('5')).await;
    policy_mut(&mut app).apply_text(Concurrency, "7").unwrap();
    key(&mut app, KeyCode::Char('1')).await;
    key(&mut app, KeyCode::Char('5')).await;
    assert!(screen(&mut app).contains("7"));
    key(&mut app, KeyCode::Char('s')).await;
    let requests = finish(task).await;
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "GetHarnessManagerPolicy")
            .count(),
        1
    );
    assert_eq!(
        requests.last().unwrap()["params"]["policy"]["max_active_sessions"],
        7
    );
}

#[tokio::test]
async fn section_strip_renders_every_section_label_and_marks_active() {
    let (mut app, config) = fixture();
    let (_dir, task) = connect(&mut app, board_cases(&config, vec![], vec![], vec![])).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    let text = screen(&mut app);
    for label in ["1 Board", "2 Decisions", "3 Inbox", "4 Inspect", "5 Policy"] {
        assert!(text.contains(label));
    }
    assert!(text.contains("[1 Board]"));
    finish(task).await;
}

#[tokio::test]
async fn decision_receipt_reloads_the_record_and_shows_its_new_state() {
    let (mut app, config) = fixture();
    let inspection = page(
        &config,
        ManagerInspectSectionV2::Decisions,
        vec![json!({
            "type":"decision", "key":"approval-target", "row_version":4,
            "payload":{"status":"pending", "question":"Approve this exact invocation?", "target_digest":"sha256:invocation"}
        })],
        None,
    );
    let reloaded = page(
        &config,
        ManagerInspectSectionV2::Decisions,
        vec![json!({
            "type":"decision", "key":"approval-target", "row_version":5,
            "payload":{"status":"answered", "answer":"allow", "question":"Approve this exact invocation?", "target_digest":"sha256:invocation"}
        })],
        None,
    );
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerState", json!({"result":inspection})),
            (
                "AnswerHarnessManagerDecision",
                json!({"result":{"state":"queued"}}),
            ),
            ("GetHarnessManagerState", json!({"result":reloaded})),
        ],
    )
    .await;
    board::open(&mut app, config, "Coordination desk".into(), true)
        .await
        .unwrap();
    key(&mut app, KeyCode::Enter).await;
    for c in "allow".chars() {
        key(&mut app, KeyCode::Char(c)).await;
    }
    key(&mut app, KeyCode::Enter).await;
    finish(task).await;
    let text = screen(&mut app);
    assert!(text.contains("Answered approval-target with “allow”"));
    assert!(text.contains("answered"));
    assert!(text.contains("CLOSED 1"));
}

#[test]
fn oversized_board_record_reports_partial_detail_coverage() {
    let row = json!({"observations": vec![json!({"accepted":null}); 600]});
    let details = board::row_details(&row);
    assert!(details[0].contains("accepted: unknown"));
    assert!(
        details
            .last()
            .unwrap()
            .contains("remaining fields are unknown")
    );
}

#[test]
fn work_board_renders_current_and_historical_db_review_receipts() {
    let source = "a".repeat(40);
    let current = Uuid::new_v4();
    let historical = Uuid::new_v4();
    let row = json!({
        "type":"work",
        "key":"db-control-plane",
        "title":"DB-native control plane",
        "source_accepted":true,
        "review":{
            "mode":"db_native",
            "source_commit":source,
            "accepted":true,
            "current":{
                "assignment_id":current,
                "state":"submitted",
                "verdict":"accepted",
                "current":true,
                "eligible":true,
                "finding_count":0,
                "blocking_finding_count":0
            },
            "historical":[{
                "assignment_id":historical,
                "state":"superseded",
                "verdict":"changes_requested",
                "current":false,
                "eligible":false,
                "finding_count":2,
                "blocking_finding_count":1
            }]
        }
    });
    let details = board::row_details(&row).join("\n");
    for expected in [
        "title: DB-native control plane",
        "review / mode: db_native",
        "review / source commit: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "review / current / assignment id:",
        "review / current / verdict: accepted",
        "review / current / eligible: true",
        "review / historical [1] / state: superseded",
        "review / historical [1] / verdict: changes_requested",
        "source accepted: true",
    ] {
        assert!(
            details.contains(expected),
            "missing positive board field: {expected}"
        );
    }
}

#[test]
fn action_rows_distinguish_container_commit_from_provider_launch() {
    let queued_epic = json!({
        "type": "action",
        "action_kind": "create_container",
        "target_type": "epic_container",
        "receipt_state": "queued",
        "state": "queued",
        "result": null
    });
    let title = board::row_title(&queued_epic);
    assert!(title.contains("Create container"));
    assert!(title.contains("Epic container"));
    assert!(title.contains("queued (admission only)"));
    let details = board::row_details(&queued_epic).join("\n");
    assert!(details.contains("Action kind: Create container"));
    assert!(details.contains("Target type: Epic container"));
    assert!(details.contains("Receipt state: queued — admission only; no completion confirmed"));

    let committed_epic = json!({
        "type": "action",
        "action_kind": "create_container",
        "target_type": "epic_container",
        "receipt_state": "succeeded",
        "state": "succeeded",
        "result": {
            "target_state": "container_committed",
            "lead_state": "unassigned"
        }
    });
    let title = board::row_title(&committed_epic);
    assert!(title.contains("Create container"));
    assert!(title.contains("Epic container"));
    assert!(title.contains("succeeded"));
    assert!(title.contains("Container committed"));
    assert!(title.contains("lead unassigned"));
    let details = board::row_details(&committed_epic).join("\n");
    for expected in [
        "Action kind: Create container",
        "Target type: Epic container",
        "Receipt state: succeeded",
        "Target state: Container committed",
        "Lead unassigned",
    ] {
        assert!(details.contains(expected), "{expected}");
    }

    let queued_provider = json!({
        "type": "action",
        "action_kind": "create_session",
        "target_type": "provider_session",
        "receipt_state": "queued",
        "state": "queued",
        "result": null
    });
    let title = board::row_title(&queued_provider);
    assert!(title.contains("Create session"));
    assert!(title.contains("Agent/provider session"));
    assert!(title.contains("queued (admission only)"));
    let details = board::row_details(&queued_provider).join("\n");
    assert!(details.contains("Receipt state: queued — admission only; no completion confirmed"));
}

#[test]
fn migration_allocation_inspect_section_and_conflict_title_are_visible() {
    assert!(board::SECTIONS.contains(&ManagerInspectSectionV2::MigrationAllocations));
    let row = json!({
        "type":"migration_allocation",
        "claim_id":Uuid::new_v4(),
        "work_key":"schema-cutover",
        "version":144,
        "state":"active",
        "conflict":true,
        "remote_tip":"b".repeat(40),
        "row_version":3,
        "source_commit":"a".repeat(40),
        "expires_at":"2026-09-27T00:00:00.000000000Z"
    });
    assert_eq!(
        board::row_title(&row),
        "schema v144 · schema-cutover · conflict"
    );
    let details = board::row_details(&row).join("\n");
    assert!(details.contains("claim id"));
    assert!(details.contains("source commit"));
    assert!(details.contains("remote tip"));
    assert!(details.contains("row version"));
    assert!(details.contains("expires at"));
}

#[tokio::test]
async fn migration_allocations_inspect_uses_operator_manager_state_rpc() {
    let (mut app, config) = fixture();
    let inspection = page(
        &config,
        ManagerInspectSectionV2::MigrationAllocations,
        vec![json!({
            "type":"migration_allocation",
            "key":"00000000000000000001",
            "sequence":1,
            "claim_id":Uuid::new_v4(),
            "work_key":"schema-cutover",
            "epic_id":config.epic_ids[0],
            "source_commit":"a".repeat(40),
            "version":144,
            "state":"active",
            "remote_tip":"b".repeat(40),
            "candidate_commit":null,
            "row_version":3,
            "expires_at":"2026-09-27T00:00:00.000000000Z",
            "conflict":true,
            "conflict_reason":"remote_tip_changed"
        })],
        None,
    );
    let (_dir, task) = connect(
        &mut app,
        vec![("GetHarnessManagerState", json!({"result":inspection}))],
    )
    .await;
    board::open_section(
        &mut app,
        config,
        "Coordination desk".into(),
        ManagerSection::Inspect(ManagerInspectSectionV2::MigrationAllocations),
    )
    .await
    .unwrap();
    let requests = finish(task).await;
    assert_eq!(
        requests[0]["params"]["query"]["section"],
        "migration_allocations"
    );
    let text = screen(&mut app);
    assert!(text.contains("Migration allocations"));
    assert!(text.contains("schema v144 · schema-cutover · conflict"));
}

#[tokio::test]
async fn unresolved_operator_gate_opens_its_exact_session_and_preserves_pending_approval() {
    let (mut app, config) = fixture();
    let mut target = baseline_session(Uuid::new_v4(), SessionKind::Feature);
    target.project_id = Some(config.project_id);
    target.title = Some("Inspect pending provider approval".into());
    let inspection = page(
        &config,
        ManagerInspectSectionV2::Decisions,
        vec![json!({
            "type":"operator_gate","key":"approval:exact","session_id":target.id,"question":"Pending tool approval",
            "state":"blocked","gate_state":"pending","route_state":"unavailable","next_action":"Open the exact session to inspect this approval."
        })],
        None,
    );
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerState", json!({"result":inspection})),
            ("GetSession", json!({"result":target})),
        ],
    )
    .await;
    board::open(&mut app, config, "Coordination desk".into(), true)
        .await
        .unwrap();
    key(&mut app, KeyCode::Char('a')).await;
    assert_eq!(
        board_state(&app).inspection.rows[0]["gate_state"],
        "pending"
    );
    assert!(screen(&mut app).contains("Open the exact session"));
    key(&mut app, KeyCode::Char('o')).await;
    let calls = finish(task).await;
    assert_eq!(calls[1]["params"]["session_id"], target.id.to_string());
    assert_eq!(app.selected_session_id(), Some(target.id));
    assert_eq!(
        app.sessions[&target.id].session.title.as_deref(),
        Some("Inspect pending provider approval")
    );
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn inherited_request_opens_current_recipient_and_retains_original_identity() {
    let (mut app, config) = fixture();
    let original = Uuid::new_v4();
    let mut current = baseline_session(Uuid::new_v4(), SessionKind::Feature);
    current.project_id = Some(config.project_id);
    current.title = Some("Current feature lead".into());
    let mut cases = board_cases(
        &config,
        vec![],
        vec![],
        vec![json!({
            "type":"request", "key":"inherited-request", "state":"retrieved",
            "recipient_session_id":original,
            "effective_recipient_session_id":current.id
        })],
    );
    cases.push(("GetSession", json!({"result":current})));
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    let row = board_state(&app).selected_row().unwrap();
    assert_eq!(row["recipient_session_id"], original.to_string());
    assert_eq!(
        row["effective_recipient_session_id"],
        current.id.to_string()
    );
    let details = board::row_details(row).join("\n");
    assert!(details.contains(&original.to_string()));
    assert!(details.contains(&current.id.to_string()));
    key(&mut app, KeyCode::Char('o')).await;
    let calls = finish(task).await;
    assert_eq!(calls[4]["params"]["session_id"], current.id.to_string());
    assert_eq!(app.selected_session_id(), Some(current.id));
    assert_eq!(
        app.sessions[&current.id].session.title.as_deref(),
        Some("Current feature lead")
    );
}

#[tokio::test]
async fn manager_control_opens_the_current_manager_and_keeps_logical_identity_visible() {
    let (mut app, config) = fixture();
    let mut current = baseline_session(Uuid::new_v4(), SessionKind::Standard);
    current.project_id = Some(config.project_id);
    current.title = Some("Current project manager".into());
    let mut cases = board_cases(
        &config,
        vec![json!({
            "type":"manager_control", "key":"manager_control:current",
            "eligibility":"eligible",
            "title":current.title,
            "logical_manager_session_id":config.manager_session_id,
            "current_session_id":current.id,
            "expected":{"authority_epoch":7,"custody_generation":1},
            "unresolved_operation_id":null
        })],
        vec![],
        vec![],
    );
    cases.push(("GetSession", json!({"result":current})));
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config.clone(), "Coordination desk".into(), false)
        .await
        .unwrap();
    let row = &board_state(&app).inspection.rows[0];
    assert!(board::row_title(row).contains("Current project manager"));
    let details = board::row_details(row).join("\n");
    assert!(details.contains("eligibility: eligible"));
    assert!(details.contains(&config.manager_session_id.to_string()));
    assert!(details.contains(&current.id.to_string()));
    assert!(details.contains("authority epoch: 7"));
    key(&mut app, KeyCode::Char('o')).await;
    let calls = finish(task).await;
    assert_eq!(calls[4]["params"]["session_id"], current.id.to_string());
    assert_eq!(app.selected_session_id(), Some(current.id));
    assert_eq!(
        app.sessions[&current.id].session.title.as_deref(),
        Some("Current project manager")
    );
}

fn board_overview_rows(config: &HarnessManagerConfigV1) -> Vec<Value> {
    vec![
        json!({"type":"overview","key":"program","kind":"program","denominator":10,"accepted":3,"integrated":2,"ready":4,"partial":1,"unknown":0,"missing_work_scope":false}),
        json!({"type":"overview","key":"product","kind":"product","denominator":5,"accepted":1,"integrated":0,"ready":2,"partial":3,"unknown":1,"missing_work_scope":true}),
        json!({"type":"lead_control","key":format!("lead:{}", config.epic_ids[0]),"epic_id":config.epic_ids[0],"title":"Orchestration Agent Process Fix","fence_state":"current"}),
        json!({"type":"intent","key":"intent:ship","title":"Ship manager board","state":"active"}),
    ]
}
fn pending_decision(key: &str, question: &str, version: i64) -> Value {
    json!({"type":"decision","key":key,"row_version":version,"payload":{"status":"pending","question":question,"target_digest":format!("sha256:{key}")}})
}
fn request_row(key: &str, message: &str) -> Value {
    json!({"type":"request","key":key,"request_id":key,"state":"retrieved","message":message})
}
fn board_surface(
    app: &mut App,
    config: &HarnessManagerConfigV1,
    overview: Vec<Value>,
    decisions: ManagerInspectionV2,
    requests: ManagerInspectionV2,
) {
    let mut ledger = board::BoardState::new(
        config.project_id,
        "Coordination desk".into(),
        AgentManagerInspectRequestV2::default(),
        page(config, ManagerInspectSectionV2::Overview, vec![], None),
    );
    let kpis = overview
        .iter()
        .filter(|row| row["type"] == "overview")
        .cloned()
        .collect();
    ledger.install_board(
        page(config, ManagerInspectSectionV2::Overview, overview, None),
        board::BoardPages {
            decisions,
            requests,
            health: page(config, ManagerInspectSectionV2::Health, vec![], None),
            kpis,
            kpis_beyond: false,
        },
    );
    app.overlay = OverlayState::HarnessManagerV2(Box::new(ManagerSurface {
        project_id: config.project_id,
        identity: "Coordination desk".into(),
        section: ManagerSection::Board,
        ledger,
        policy: None,
        config: config.clone(),
    }));
}

fn health_row(epic: Uuid, title: &str) -> Value {
    json!({"type":"health","key":epic,"epic_id":epic,"title":title,
        "lead":{"session_id":Uuid::new_v4(),"status":"Completed","age_seconds":1020},
        "lead_state":"current",
        "children":{"live":2},
        "notices":{"to_manager":{"pending":2},"to_lead":{"pending":1}},
        "reports":{"pending_requests":3,"informational_messages":7},
        "stuck":[{"code":"manager_watch_missing","evidence":[epic]},
                 {"code":"lead_delivery_abandoned","evidence":["job-7"]}],
        "complete":true,"truncated":[]})
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_renders_health_band_with_epic_and_its_stuck_codes() {
    let (mut app, config) = fixture();
    let epic = config.epic_ids[0];
    let mut cases = board_cases(&config, vec![], vec![], vec![]);
    cases[3].1 = json!({"result":page(
        &config,
        ManagerInspectSectionV2::Health,
        vec![health_row(epic, "Fleet health Epic")],
        None,
    )});
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    let requests = finish(task).await;
    assert_eq!(requests[3]["params"]["query"]["section"], "health");
    assert!(
        board::band_row_label(board::Band::Health, &health_row(epic, "Fleet health Epic"))
            .contains("3 pending/7 info")
    );
    let row = board_state(&app).selected_row().unwrap();
    assert_eq!(
        board::band_row_label(board::Band::Health, row),
        "3 pending/7 info · Fleet health Epic · stuck manager_watch_missing,lead_delivery_abandoned \
         · lead Completed 17m · live 2 · notices 3"
    );
    let text = screen(&mut app);
    for value in [
        "HEALTH 1",
        "3 pending/7 info · Fleet health Epic",
        "code: lead_delivery_abandoned",
        &format!("epic id: {epic}"),
        "coverage complete",
    ] {
        assert!(text.contains(value), "{value} missing from board");
    }
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_health_band_names_codes_for_a_failed_lead_instead_of_ok() {
    let (mut app, config) = fixture();
    let epic = config.epic_ids[0];
    let mut failed = health_row(epic, "Failed lead Epic");
    failed["lead"]["status"] = json!("Failed");
    failed["lead"]["age_seconds"] = json!(300);
    failed["stuck"] = json!([{"code":"lead_unavailable","evidence":[epic,"Failed"]}]);
    // A row reporting no codes while its lead is Interrupted (for example from
    // a daemon without lead_unavailable) is unverified, never ok.
    let mut quiet = health_row(Uuid::new_v4(), "Interrupted lead Epic");
    quiet["lead"]["status"] = json!("Interrupted");
    quiet["lead"]["age_seconds"] = json!(120);
    quiet["stuck"] = json!([]);
    let mut cases = board_cases(&config, vec![], vec![], vec![]);
    cases[3].1 = json!({"result":page(
        &config,
        ManagerInspectSectionV2::Health,
        vec![failed.clone(), quiet.clone()],
        None,
    )});
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    finish(task).await;
    assert_eq!(
        board::band_row_label(board::Band::Health, &failed),
        "3 pending/7 info · Failed lead Epic · stuck lead_unavailable · lead Failed 5m · live 2 · notices 3"
    );
    assert_eq!(
        board::band_row_label(board::Band::Health, &quiet),
        "3 pending/7 info · Interrupted lead Epic · unverified · lead Interrupted 2m · live 2 · notices 3"
    );
    let text = screen(&mut app);
    for value in [
        "HEALTH 2",
        "3 pending/7 info · Failed",
        "3 pending/7 info · Interrupted",
        "code: lead_unavailable",
    ] {
        assert!(text.contains(value), "{value} missing from board");
    }
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_renders_kpis_and_banded_decisions_requests_leads() {
    let (mut app, config) = fixture();
    let (_dir, task) = connect(
        &mut app,
        board_cases(
            &config,
            board_overview_rows(&config),
            vec![pending_decision(
                "release-target",
                "Choose release target",
                7,
            )],
            vec![request_row("req-1", "Please rebase the lead onto rolling")],
        ),
    )
    .await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    let requests = finish(task).await;
    let fetched = requests
        .iter()
        .map(|r| r["params"]["query"]["section"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(fetched, ["overview", "decisions", "requests", "health"]);
    let text = screen(&mut app);
    for value in [
        "Coordination desk",
        "scope Selected · 1 Epics · policy Status",
        "coverage complete",
        "program accepted 3 · integrated 2 / 10 · ready 4 · partial 1 · unknown 0",
        "product accepted 1 · integrated 0 / 5 · ready 2 · partial 3 · unknown 1 · missing work scope",
        "DECISIONS 1",
        "REQUESTS 1",
        "LEADS 1",
        "SIGNALS 1",
        "Choose release target · needs answer",
        "Please rebase the lead onto rolling",
        "Orchestration Agent Process Fix · fence current",
        "Ship manager board · active",
    ] {
        assert!(text.contains(value), "{value}");
    }
    // j/k crosses band boundaries in display order.
    let mut origins = vec![board_state(&app).selected_origin()];
    for _ in 0..3 {
        key(&mut app, KeyCode::Char('j')).await;
        origins.push(board_state(&app).selected_origin());
    }
    assert_eq!(
        origins,
        [
            Some((ManagerInspectSectionV2::Decisions, 0)),
            Some((ManagerInspectSectionV2::Requests, 0)),
            Some((ManagerInspectSectionV2::Overview, 2)),
            Some((ManagerInspectSectionV2::Overview, 3)),
        ]
    );
    key(&mut app, KeyCode::Char('k')).await;
    assert!(screen(&mut app).contains("fence state: current"));
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_answers_decision_row_in_place_with_exact_fences() {
    let (mut app, config) = fixture();
    // The Decisions page carries newer fences than the Overview page: the
    // draft must use the row's origin page, not the Board's own query.
    let mut decisions = page(
        &config,
        ManagerInspectSectionV2::Decisions,
        vec![
            pending_decision("first-gate", "Approve first gate?", 4),
            pending_decision("release-target", "Choose release target", 7),
        ],
        None,
    );
    decisions.scope_version = 5;
    let policy = decisions.policy.as_mut().unwrap();
    policy.scope_version = 5;
    policy.row_version = 9;
    let mut cases = board_cases(&config, board_overview_rows(&config), vec![], vec![]);
    cases[1].1 = json!({"result":decisions});
    cases.push((
        "AnswerHarnessManagerDecision",
        json!({"result":{"state":"queued"}}),
    ));
    // The answer reloads the whole Board.
    cases.extend(board_cases(
        &config,
        board_overview_rows(&config),
        vec![],
        vec![],
    ));
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    key(&mut app, KeyCode::Char('j')).await;
    key(&mut app, KeyCode::Char('a')).await;
    for c in "rolling".chars() {
        key(&mut app, KeyCode::Char(c)).await;
    }
    let expected = board_state(&app).answer.as_ref().unwrap().target.clone();
    key(&mut app, KeyCode::Enter).await;
    let requests = finish(task).await;
    assert_eq!(requests[4]["method"], "AnswerHarnessManagerDecision");
    assert_eq!(
        requests[4]["params"],
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(expected.decision_key, "release-target");
    assert_eq!(expected.expected_row_version, 7);
    assert_eq!(expected.target_digest, "sha256:release-target");
    assert_eq!(expected.fence.scope_version, 5);
    assert_eq!(expected.fence.policy_version, 9);
    assert_eq!(expected.answer, "rolling");
    assert!(matches!(
        app.overlay,
        OverlayState::HarnessManagerV2(ref s) if s.section == ManagerSection::Board
    ));
    assert!(screen(&mut app).contains("Answered release-target with “rolling”"));
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_enter_drills_into_owning_section_with_row_selected() {
    let (mut app, config) = fixture();
    let (_dir, task) = connect(
        &mut app,
        board_cases(
            &config,
            board_overview_rows(&config),
            vec![],
            vec![
                request_row("req-1", "Please rebase the lead onto rolling"),
                request_row("req-2", "Confirm the release checklist"),
            ],
        ),
    )
    .await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    key(&mut app, KeyCode::Char('j')).await;
    key(&mut app, KeyCode::Enter).await;
    let OverlayState::HarnessManagerV2(view) = &app.overlay else {
        panic!("surface closed");
    };
    assert_eq!(view.section, ManagerSection::Inbox);
    assert_eq!(view.ledger.query.section, ManagerInspectSectionV2::Requests);
    assert_eq!(view.ledger.selected, 1);
    assert_eq!(view.ledger.selected_row().unwrap()["key"], "req-2");
    let text = screen(&mut app);
    assert!(text.contains("[3 Inbox]"));
    assert!(text.contains("Confirm the release checklist"));
    // Drill-down reuses the loaded first page: only the Board's four loads.
    assert_eq!(finish(task).await.len(), 4);
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_enter_on_lead_row_drills_into_overview_inspect() {
    let (mut app, config) = fixture();
    let (_dir, task) = connect(
        &mut app,
        board_cases(&config, board_overview_rows(&config), vec![], vec![]),
    )
    .await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    key(&mut app, KeyCode::Enter).await;
    finish(task).await;
    let OverlayState::HarnessManagerV2(view) = &app.overlay else {
        panic!("surface closed");
    };
    assert_eq!(
        view.section,
        ManagerSection::Inspect(ManagerInspectSectionV2::Overview)
    );
    assert_eq!(view.ledger.selected, 2);
    assert_eq!(
        view.ledger.selected_row().unwrap()["title"],
        "Orchestration Agent Process Fix"
    );
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_band_overflow_shows_more_hint() {
    let (mut app, config) = fixture();
    let decisions = (0..board::BAND_ROWS + 2)
        .map(|i| pending_decision(&format!("gate-{i}"), &format!("Approve gate {i}?"), 1))
        .collect::<Vec<_>>();
    let mut cases = board_cases(&config, vec![], decisions, vec![]);
    cases[2].1 = json!({"result":page(
        &config,
        ManagerInspectSectionV2::Requests,
        vec![request_row("req-1", "Please rebase the lead onto rolling")],
        Some("next-requests"),
    )});
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    finish(task).await;
    let text = screen(&mut app);
    for value in [
        "DECISIONS 8",
        "Approve gate 5?",
        "REQUESTS 1+",
        "+more · Enter here opens full Decisions (2)",
        "+more · Enter here opens full Inbox (3)",
        "coverage requests +more",
    ] {
        assert!(text.contains(value), "{value}");
    }
    // Rows plus the two selectable `+more` lines.
    assert_eq!(board_state(&app).row_count(), board::BAND_ROWS + 1 + 2);
}

#[test]
#[allow(clippy::unwrap_used)]
fn board_band_headers_use_theme_roles() {
    crate::ui::theme::with_theme_state(|| {
        let mut observed = vec![];
        let mut dump = String::new();
        for theme_name in ["goth", "gruvbox-warm"] {
            assert!(crate::ui::theme::set_theme_by_name(theme_name));
            let (mut app, config) = fixture();
            board_surface(
                &mut app,
                &config,
                board_overview_rows(&config),
                page(
                    &config,
                    ManagerInspectSectionV2::Decisions,
                    vec![pending_decision(
                        "release-target",
                        "Choose release target",
                        7,
                    )],
                    None,
                ),
                page(
                    &config,
                    ManagerInspectSectionV2::Requests,
                    vec![request_row("req-1", "Please rebase the lead onto rolling")],
                    None,
                ),
            );
            let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
            terminal
                .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let area = buffer.area;
            let find = |needle: &str| -> ratatui::style::Color {
                let chars: Vec<String> = needle.chars().map(String::from).collect();
                for y in 0..area.height {
                    let row: Vec<&str> = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
                    if let Some(x) = row.windows(chars.len()).position(|w| w == chars.as_slice()) {
                        return buffer[(u16::try_from(x).unwrap(), y)].fg;
                    }
                }
                panic!("{needle} not rendered under {theme_name}");
            };
            let colors = [
                (find("DECISIONS 1"), crate::ui::theme::status_waiting()),
                (find("REQUESTS 1"), crate::ui::theme::status_running()),
                (find("LEADS 1"), crate::ui::theme::teal()),
                (find("SIGNALS 1"), crate::ui::theme::warning_status()),
            ];
            for (rendered, role) in colors {
                assert_eq!(rendered, role, "{theme_name}");
            }
            observed.push(colors.map(|(c, _)| c));
            {
                use std::fmt::Write as _;
                writeln!(
                    dump,
                    "--- theme {theme_name}: band fg {:?}",
                    observed.last().unwrap()
                )
                .unwrap();
            }
            for y in 0..area.height {
                let line: String = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
                dump.push_str(line.trim_end());
                dump.push('\n');
            }
        }
        assert_ne!(
            observed[0], observed[1],
            "themes resolve distinct band roles"
        );
        if std::env::var_os("RSI_BOARD_DUMP").is_some() {
            println!("{dump}");
        }
    });
}

fn signal_rows(prefix: &str, count: usize) -> Vec<Value> {
    (0..count)
        .map(|i| json!({"type":"intent","key":format!("intent:{prefix}{i:03}"),"title":format!("Signal {prefix}{i}"),"state":"active"}))
        .collect()
}
fn overview_page_case(
    config: &HarnessManagerConfigV1,
    rows: Vec<Value>,
    next: Option<&str>,
) -> Case {
    (
        "GetHarnessManagerState",
        json!({"result":page(config, ManagerInspectSectionV2::Overview, rows, next)}),
    )
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_kpis_follow_overview_pages_past_earlier_sorting_signals() {
    let (mut app, config) = fixture();
    // 32 `intent:*` keys sort before `product`/`program` and fill page one.
    let kpis = board_overview_rows(&config)[..2].to_vec();
    let mut cases = vec![
        overview_page_case(&config, signal_rows("a", 32), Some("overview-2")),
        overview_page_case(&config, kpis, None),
    ];
    cases.extend(
        board_cases(&config, vec![], vec![], vec![])
            .into_iter()
            .skip(1),
    );
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    let requests = finish(task).await;
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[1]["params"]["query"]["cursor"], "overview-2");
    let text = screen(&mut app);
    for value in [
        "program accepted 3 · integrated 2 / 10 · ready 4 · partial 1 · unknown 0",
        "product accepted 1 · integrated 0 / 5 · ready 2 · partial 3 · unknown 1",
        "SIGNALS 32+",
    ] {
        assert!(text.contains(value), "{value}");
    }
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_kpis_beyond_page_bound_are_reported_explicitly() {
    let (mut app, config) = fixture();
    let mut cases = (0..board::KPI_OVERVIEW_PAGES)
        .map(|i| {
            overview_page_case(
                &config,
                signal_rows(&format!("p{i}-"), 32),
                Some(["o2", "o3", "o4", "o5"][i]),
            )
        })
        .collect::<Vec<_>>();
    cases.extend(
        board_cases(&config, vec![], vec![], vec![])
            .into_iter()
            .skip(1),
    );
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    let requests = finish(task).await;
    let overview_calls = requests
        .iter()
        .filter(|r| r["params"]["query"]["section"] == "overview")
        .count();
    assert_eq!(overview_calls, board::KPI_OVERVIEW_PAGES);
    assert!(screen(&mut app).contains("KPIs beyond 4 Overview pages not loaded"));
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_empty_band_with_more_pages_opens_its_full_section() {
    let (mut app, config) = fixture();
    let mut cases = board_cases(&config, vec![], vec![], vec![]);
    cases[1].1 = json!({"result":page(
        &config,
        ManagerInspectSectionV2::Decisions,
        vec![],
        Some("decisions-2"),
    )});
    cases.push((
        "GetHarnessManagerState",
        json!({"result":page(
            &config,
            ManagerInspectSectionV2::Decisions,
            vec![pending_decision("later-gate", "Approve later gate?", 2)],
            None,
        )}),
    ));
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    let text = screen(&mut app);
    assert!(text.contains("DECISIONS 0+"));
    assert!(text.contains("+more · Enter here opens full Decisions (2)"));
    assert_eq!(
        board_state(&app).entries().first(),
        Some(&board::BoardEntry::More(board::Band::Decisions))
    );
    key(&mut app, KeyCode::Enter).await;
    let OverlayState::HarnessManagerV2(view) = &app.overlay else {
        panic!("surface closed");
    };
    assert_eq!(view.section, ManagerSection::Decisions);
    assert_eq!(
        view.ledger.inspection.next_cursor.as_deref(),
        Some("decisions-2")
    );
    key(&mut app, KeyCode::Char('n')).await;
    let requests = finish(task).await;
    assert_eq!(requests[4]["params"]["query"]["section"], "decisions");
    assert_eq!(requests[4]["params"]["query"]["cursor"], "decisions-2");
    assert!(screen(&mut app).contains("Approve later gate?"));
}

#[test]
#[allow(clippy::unwrap_used)]
fn board_header_keeps_coverage_when_identity_wraps_past_five_rows() {
    let (mut app, config) = fixture();
    board_surface(
        &mut app,
        &config,
        board_overview_rows(&config),
        page(&config, ManagerInspectSectionV2::Decisions, vec![], None),
        page(&config, ManagerInspectSectionV2::Requests, vec![], None),
    );
    let identity = "Coordination desk for the long-running release program ".repeat(8);
    if let OverlayState::HarnessManagerV2(view) = &mut app.overlay {
        view.ledger.identity = identity;
    }
    let mut terminal = Terminal::new(TestBackend::new(100, 34)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                + "\n"
        })
        .collect();
    assert!(
        text.contains("Coordination desk for the long-running"),
        "{text}"
    );
    assert!(text.contains("coverage complete"), "{text}");
    assert!(
        text.contains("scope Selected · 1 Epics · policy Status"),
        "{text}"
    );
    assert!(text.contains("program accepted 3"), "{text}");
}

#[test]
#[allow(clippy::unwrap_used)]
fn board_header_wraps_long_identity_and_keeps_coverage() {
    let (mut app, config) = fixture();
    board_surface(
        &mut app,
        &config,
        board_overview_rows(&config),
        page(&config, ManagerInspectSectionV2::Decisions, vec![], None),
        page(&config, ManagerInspectSectionV2::Requests, vec![], None),
    );
    if let OverlayState::HarnessManagerV2(view) = &mut app.overlay {
        view.ledger.identity = format!(
            "{} · Orchestration Agent Process Fix project",
            "Coordination desk for the long-running release program ".repeat(2)
        );
    }
    let mut terminal = Terminal::new(TestBackend::new(100, 34)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let lines: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect();
    assert!(
        lines.iter().any(|line| line.contains("coverage")),
        "{lines:#?}"
    );
    assert!(lines.iter().any(|line| line.contains("complete")));
    assert!(lines.iter().any(|line| line.contains("program accepted 3")));
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn manager_surface_help_lists_section_keys_close_and_decision_answer() {
    use crate::action_registry::{HelpOrigin, OverlayHelpClass};
    let (mut app, config) = fixture();
    let cases = board_cases(&config, vec![], vec![], vec![]);
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    finish(task).await;

    let help = |app: &mut App| {
        crate::overlay::keybindings_help::open_contextual_help(app);
        let origin = match &app.overlay {
            OverlayState::KeybindingsHelp { origin, .. } => *origin,
            _ => panic!("help did not open"),
        };
        let text = crate::ui::overlay::keybindings_help::contextual_lines(app, "")
            .iter()
            .map(|line| format!("{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        crate::overlay::keybindings_help::close_keybindings_help(app);
        (origin, text)
    };
    let (origin, text) = help(&mut app);
    assert_eq!(
        origin,
        HelpOrigin::OverlayClass(OverlayHelpClass::ManagerBoard)
    );
    for snippet in [
        "Close manager",
        "Board / Decisions / Inbox / Inspect / Policy",
        "Open the selected row's session",
    ] {
        assert!(text.contains(snippet), "{snippet}: {text}");
    }
    assert!(matches!(app.overlay, OverlayState::HarnessManagerV2(..)));

    surface_mut(&mut app).unwrap().section = ManagerSection::Decisions;
    let (origin, text) = help(&mut app);
    assert_eq!(
        origin,
        HelpOrigin::OverlayClass(OverlayHelpClass::ManagerDecisions)
    );
    assert!(text.contains("Answer the highlighted option"), "{text}");
    assert!(text.contains("Accept the asker's recommendation"), "{text}");
    assert!(text.contains("Close manager"), "{text}");
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn board_header_shows_manager_seat_state() {
    let (mut app, config) = fixture();
    let mut overview = board_overview_rows(&config);
    for row in overview.iter_mut().filter(|row| row["type"] == "overview") {
        row["manager_available"] = json!(true);
        row["manager_seat"] = json!({"state":"exhausted","tip_session_id":config.manager_session_id,
            "since":"2026-09-23T10:54:18.374000000Z","attempts":2,"max_attempts":2,
            "reason":"manager_seat_recovery_budget_exhausted","next_action":"resume, appoint or succeed"});
    }
    board_surface(
        &mut app,
        &config,
        overview,
        page(&config, ManagerInspectSectionV2::Decisions, vec![], None),
        page(&config, ManagerInspectSectionV2::Requests, vec![], None),
    );
    let text = screen(&mut app);
    assert!(
        text.contains("seat EXHAUSTED 2/2 (manager_seat_recovery_budget_exhausted)"),
        "{text}"
    );
    assert!(text.contains("Coordination desk"), "{text}");
    assert!(text.contains("coverage complete"), "{text}");
}

/// #674 K15b: the manager-wide Resources row as the daemon reports it.
fn usage_resources_row() -> Value {
    json!({"type":"resources","key":"cohort","active_sessions":3,
        "created_sessions":{"used":152,"limit":1024,"remaining":872},
        "cohort_complete":true})
}

fn loaded_usage() -> policy::usage::CreationUsage {
    policy::usage::CreationUsage {
        created: 152,
        created_limit: Some(1024),
        created_remaining: Some(872),
        active: 3,
    }
}

fn focus_field(app: &mut App, field: policy::Field) {
    let s = policy_mut(app);
    s.selected = s.rows().iter().position(|r| r.field == field).unwrap();
}

#[tokio::test]
async fn policy_editor_renders_titled_sections_usage_header_and_row_help() {
    let (mut app, config) = fixture();
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
    finish(task).await;
    policy_mut(&mut app).apply_usage(Ok(Ok(loaded_usage())));
    let top = screen(&mut app);
    for text in [
        "── Authority · presets ──",
        "── Budgets ──",
        "── Launches ──",
        "Usage: sessions 152/1024 created · 872 left · active 3/4",
        "Created session quota: 0",
        "Active session limit: 4",
        "Enter runs · Replace mode and grants with this preset; explicit limits are kept",
    ] {
        assert!(top.contains(text), "{text}\n{top}");
    }
    focus_field(&mut app, policy::Field::Retries);
    let recovery = screen(&mut app);
    for text in [
        "── Recovery ──",
        "Recovery attempt limit: 0",
        "Enter edits · range 0–32 · Automatic recovery attempts per Epic; 0 disables automatic recovery",
    ] {
        assert!(recovery.contains(text), "{text}\n{recovery}");
    }
    focus_field(&mut app, policy::Field::Detail(0));
    let preview = screen(&mut app);
    assert!(
        preview.contains("── Effective preview · read-only ──"),
        "{preview}"
    );
    assert!(
        preview.contains("read-only · Read-only: computed from the draft"),
        "{preview}"
    );
}

#[tokio::test]
async fn policy_every_accepted_field_is_a_top_level_editable_row() {
    use policy::{Field::*, RowKind};
    let (mut app, config) = fixture();
    let (_dir, task) = connect(
        &mut app,
        vec![(
            "GetHarnessManagerPolicy",
            json!({"result":policy_config(&config)}),
        )],
    )
    .await;
    policy::open(&mut app, config.clone(), "Coordination desk".into())
        .await
        .unwrap();
    finish(task).await;
    let s = policy_mut(&mut app);
    assert!(!s.advanced_open);
    let rows = s.rows();
    let kind = |field: policy::Field| {
        rows.iter()
            .find(|r| r.field == field)
            .unwrap_or_else(|| panic!("{field:?} is a top-level row"))
            .kind
    };
    let mut editable = vec![
        Mode,
        Paused,
        Epic(config.epic_ids[0]),
        AddPausedEpic,
        AddGroup,
        CreateGroups,
        Sessions,
        Containers,
        Concurrency,
        Spend,
        Retries,
        RetryDelay,
        Deadline,
        AddLaunch,
    ];
    editable.extend((0..10).map(Capability));
    editable.extend((0..policy::PROVIDERS.len()).map(ProviderLimit));
    for field in editable {
        assert!(
            matches!(
                kind(field),
                RowKind::Edit | RowKind::Toggle | RowKind::Action
            ),
            "{field:?}"
        );
    }
    assert_eq!(kind(Detail(0)), RowKind::ReadOnly);
}

#[tokio::test]
async fn created_session_quota_edits_above_256_marks_unsaved_and_save_states_grants() {
    let (mut app, config) = fixture();
    let mut stored = policy_config(&config);
    stored.policy.max_created_sessions = 160;
    let mut reply = stored.clone();
    reply.row_version = 3;
    reply.policy.max_created_sessions = 512;
    reply.policy.mode = ManagerOperatingModeV2::Execute;
    reply.policy.capabilities = vec![
        ManagerCapabilityV2::WorkPlan,
        ManagerCapabilityV2::SessionCreate,
    ];
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":stored})),
            ("ConfigureHarnessManagerPolicy", json!({"result":reply})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    assert!(screen(&mut app).contains("Created session quota: 160"));
    focus_field(&mut app, policy::Field::Sessions);
    key(&mut app, KeyCode::Enter).await;
    assert!(
        crate::overlay::handle_overlay_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
        )
        .await
    );
    for c in "512".chars() {
        key(&mut app, KeyCode::Char(c)).await;
    }
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(policy_mut(&mut app).draft.max_created_sessions, 512);
    let row = policy_mut(&mut app)
        .rows()
        .into_iter()
        .find(|r| r.field == policy::Field::Sessions)
        .unwrap();
    assert!(row.modified);
    let lines = screen_lines(&mut app);
    let line = lines
        .lines()
        .find(|l| l.contains("Created session quota: 512"))
        .unwrap_or_else(|| panic!("{lines}"));
    // Draft-versus-saved marker and the saved value on the edited row.
    assert!(line.contains("› *"), "{line}");
    assert!(
        line.contains("Created session quota: 512 (saved 160)"),
        "{line}"
    );
    key(&mut app, KeyCode::Char('s')).await;
    let requests = finish(task).await;
    assert_eq!(requests[1]["params"]["policy"]["max_created_sessions"], 512);
    let text = screen(&mut app);
    assert!(text.contains("Policy saved as"), "{text}");
    assert!(
        text.contains("mode Execute · grants WorkPlan, SessionCreate"),
        "{text}"
    );
    assert!(text.contains("Created session quota: 512"), "{text}");
}

#[tokio::test]
async fn policy_out_of_range_entry_and_invalid_draft_show_the_field_error_inline() {
    let (mut app, config) = fixture();
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
    focus_field(&mut app, policy::Field::Sessions);
    key(&mut app, KeyCode::Enter).await;
    for c in "2000".chars() {
        key(&mut app, KeyCode::Char(c)).await;
    }
    key(&mut app, KeyCode::Enter).await;
    let s = policy_mut(&mut app);
    assert_eq!(s.draft.max_created_sessions, 0);
    assert_eq!(s.edit.as_ref().map(|e| e.0), Some(policy::Field::Sessions));
    assert!(screen(&mut app).contains("Enter a whole number from 0 to 1024."));
    key(&mut app, KeyCode::Esc).await;
    // A draft that fails daemon validation names the field and selects it
    // instead of sending a save that would be refused.
    policy_mut(&mut app).draft.allow_create_groups = true;
    key(&mut app, KeyCode::Char('s')).await;
    finish(task).await;
    let s = policy_mut(&mut app);
    let selected = s.rows()[s.selected].field;
    assert_eq!(selected, policy::Field::CreateGroups);
    let text = screen(&mut app);
    assert!(
        text.contains("Policy not saved: Allow root Group creation needs Grant Topology."),
        "{text}"
    );
    assert!(
        text.contains("Allow root Group creation: true (saved false)  ✗ needs Grant Topology"),
        "{text}"
    );
}

#[test]
fn policy_field_bounds_match_daemon_validation() {
    use policy::Field;
    let set = |field: Field, value: u32| {
        let mut p = ManagerPolicyV2::default();
        let small = u16::try_from(value).unwrap_or(u16::MAX);
        match field {
            Field::Sessions => p.max_created_sessions = small,
            Field::Containers => p.max_created_containers = small,
            Field::Concurrency => p.max_active_sessions = small,
            Field::ProviderLimit(_) => p.provider_limits.push(ManagerProviderLimitV2 {
                provider: policy::PROVIDERS[0],
                max_active: small,
            }),
            Field::Retries => p.max_recovery_attempts = small,
            Field::RetryDelay => p.retry_delay_seconds = value,
            Field::Deadline => p.request_timeout_seconds = value,
            _ => unreachable!(),
        }
        p.validate()
    };
    for field in [
        Field::Sessions,
        Field::Containers,
        Field::Concurrency,
        Field::ProviderLimit(0),
        Field::Retries,
        Field::RetryDelay,
        Field::Deadline,
    ] {
        let (min, max) = policy::numeric_bounds(field).unwrap();
        assert!(set(field, min).is_ok(), "{field:?} min {min}");
        assert!(set(field, max).is_ok(), "{field:?} max {max}");
        assert!(set(field, max + 1).is_err(), "{field:?} above {max}");
        if min > 0 {
            assert!(set(field, min - 1).is_err(), "{field:?} below {min}");
        }
    }
}

#[tokio::test]
async fn policy_usage_request_reads_created_sessions_from_inspect_resources() {
    let (_, config) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let reply = page(
        &config,
        ManagerInspectSectionV2::Resources,
        vec![usage_resources_row()],
        None,
    );
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        let response = json!({"jsonrpc":"2.0","id":request["id"].clone(),"result":reply});
        write
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        request
    });
    let mut request = policy::usage::request_usage(path, config.project_id);
    let usage = tokio::time::timeout(std::time::Duration::from_secs(5), &mut request.receiver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let sent = server.await.unwrap();
    assert_eq!(sent["method"], "GetHarnessManagerState");
    assert_eq!(sent["params"]["query"]["section"], "resources");
    assert_eq!(usage, loaded_usage());
}

#[tokio::test]
async fn entering_policy_section_starts_background_usage_load() {
    let (mut app, config) = fixture();
    let mut cases = board_cases(&config, vec![], vec![], vec![]);
    cases.push((
        "GetHarnessManagerPolicy",
        json!({"result":policy_config(&config)}),
    ));
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    key(&mut app, KeyCode::Char('5')).await;
    finish(task).await;
    assert!(policy_mut(&mut app).usage_request.is_some());
    assert!(screen(&mut app).contains("Usage: loading · 5 refreshes"));
}

fn screen_lines(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 42)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// K15B-1: a usage snapshot as loaded under a saved quota of 160.
fn usage_under_160() -> policy::usage::CreationUsage {
    policy::usage::CreationUsage {
        created: 152,
        created_limit: Some(160),
        created_remaining: Some(8),
        active: 3,
    }
}

#[tokio::test]
async fn policy_save_renders_usage_against_the_saved_quota_immediately() {
    let (mut app, config) = fixture();
    let mut stored = policy_config(&config);
    stored.policy.max_created_sessions = 160;
    let mut reply = stored.clone();
    reply.row_version = 3;
    reply.policy.max_created_sessions = 512;
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":stored})),
            ("ConfigureHarnessManagerPolicy", json!({"result":reply})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    policy_mut(&mut app).apply_usage(Ok(Ok(usage_under_160())));
    let before = screen(&mut app);
    assert!(
        before.contains("Usage: sessions 152/160 created · 8 left · active 3/4"),
        "{before}"
    );
    policy_mut(&mut app)
        .apply_text(policy::Field::Sessions, "512")
        .unwrap();
    key(&mut app, KeyCode::Char('s')).await;
    finish(task).await;
    let after = screen(&mut app);
    assert!(
        after.contains("Usage: sessions 152/512 created · 360 left · active 3/4"),
        "{after}"
    );
}

#[tokio::test]
async fn policy_reload_keeps_same_scope_usage_and_marks_changed_scope_usage_stale() {
    let (mut app, config) = fixture();
    let mut stored = policy_config(&config);
    stored.policy.max_created_sessions = 160;
    let mut moved = config.clone();
    moved.row_version = 4;
    let mut moved_policy = stored.clone();
    moved_policy.scope_version = 4;
    let (_dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":stored})),
            ("GetHarnessManager", json!({"result":config})),
            ("GetHarnessManagerPolicy", json!({"result":stored})),
            ("GetHarnessManager", json!({"result":moved})),
            ("GetHarnessManagerPolicy", json!({"result":moved_policy})),
        ],
    )
    .await;
    policy::open(&mut app, config.clone(), "Coordination desk".into())
        .await
        .unwrap();
    policy_mut(&mut app).apply_usage(Ok(Ok(usage_under_160())));
    key(&mut app, KeyCode::Char('r')).await;
    let same = screen(&mut app);
    assert!(
        same.contains("Usage: sessions 152/160 created · 8 left · active 3/4"),
        "{same}"
    );
    key(&mut app, KeyCode::Char('r')).await;
    finish(task).await;
    assert_eq!(policy_mut(&mut app).config.row_version, 4);
    let changed = screen(&mut app);
    assert!(
        changed.contains("Usage: scope changed · 5 refreshes"),
        "{changed}"
    );
}

#[tokio::test]
async fn policy_ninth_provider_limit_is_refused_and_flagged_on_its_row() {
    use policy::Field::ProviderLimit;
    let (mut app, config) = fixture();
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
    let last = policy::PROVIDERS.len() - 1;
    {
        let s = policy_mut(&mut app);
        for i in 0..last {
            s.apply_text(ProviderLimit(i), "1").unwrap();
        }
        let error = s.apply_text(ProviderLimit(last), "1").unwrap_err();
        assert!(error.contains("At most 8 provider limits"), "{error}");
        assert_eq!(s.draft.provider_limits.len(), 8);
        // A ninth entry already in the draft (for example a retained policy)
        // is flagged on its own row and named before any save is sent.
        s.draft.provider_limits.push(ManagerProviderLimitV2 {
            provider: policy::PROVIDERS[last],
            max_active: 1,
        });
    }
    key(&mut app, KeyCode::Char('s')).await;
    finish(task).await;
    let s = policy_mut(&mut app);
    assert_eq!(s.rows()[s.selected].field, ProviderLimit(last));
    let text = screen(&mut app);
    assert!(
        text.contains(
            "Policy not saved: Harness active limit at most 8 provider limits; blank one."
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "Harness active limit: 1 (saved inherit)  ✗ at most 8 provider limits; blank one"
        ),
        "{text}"
    );
}

/// K15B-1-RACE: a snapshot taken after the quota moved to 512.
fn usage_after_512() -> policy::usage::CreationUsage {
    policy::usage::CreationUsage {
        created: 153,
        created_limit: Some(512),
        created_remaining: Some(359),
        active: 3,
    }
}

#[tokio::test]
async fn policy_usage_request_started_before_save_is_dropped_and_the_later_snapshot_applies() {
    let (mut app, config) = fixture();
    let mut stored = policy_config(&config);
    stored.policy.max_created_sessions = 160;
    let mut reply = stored.clone();
    reply.row_version = 3;
    reply.policy.max_created_sessions = 512;
    let (dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":stored})),
            ("ConfigureHarnessManagerPolicy", json!({"result":reply})),
        ],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    {
        let s = policy_mut(&mut app);
        s.apply_usage(Ok(Ok(usage_under_160())));
        // A refresh is in flight when the operator saves.
        s.start_usage(dir.path().join("usage-in-flight.sock"));
        s.apply_text(policy::Field::Sessions, "512").unwrap();
    }
    key(&mut app, KeyCode::Char('s')).await;
    finish(task).await;
    // The pre-save snapshot arrives after the save.
    policy_mut(&mut app).apply_usage(Ok(Ok(usage_under_160())));
    let raced = screen(&mut app);
    assert!(
        raced.contains("Usage: sessions 152/512 created · 360 left · active 3/4 · refreshing"),
        "{raced}"
    );
    let s = policy_mut(&mut app);
    assert_eq!(
        s.usage_request.as_ref().map(|r| r.generation),
        Some(s.usage_generation)
    );
    // The snapshot requested after the save is applied.
    s.apply_usage(Ok(Ok(usage_after_512())));
    let fresh = screen(&mut app);
    assert!(
        fresh.contains("Usage: sessions 153/512 created · 359 left · active 3/4"),
        "{fresh}"
    );
}

#[tokio::test]
async fn policy_usage_request_retained_across_same_scope_reload_is_dropped_and_the_later_snapshot_applies()
 {
    let (mut app, config) = fixture();
    let mut stored = policy_config(&config);
    stored.policy.max_created_sessions = 160;
    let mut reloaded = stored.clone();
    reloaded.row_version = 3;
    reloaded.policy.max_created_sessions = 512;
    let (dir, task) = connect(
        &mut app,
        vec![
            ("GetHarnessManagerPolicy", json!({"result":stored})),
            ("GetHarnessManager", json!({"result":config})),
            ("GetHarnessManagerPolicy", json!({"result":reloaded})),
        ],
    )
    .await;
    policy::open(&mut app, config.clone(), "Coordination desk".into())
        .await
        .unwrap();
    {
        let s = policy_mut(&mut app);
        s.apply_usage(Ok(Ok(usage_under_160())));
        s.start_usage(dir.path().join("usage-in-flight.sock"));
    }
    key(&mut app, KeyCode::Char('r')).await;
    finish(task).await;
    assert_eq!(policy_mut(&mut app).config.row_version, config.row_version);
    // The request kept across the reload finishes with its pre-reload snapshot.
    policy_mut(&mut app).apply_usage(Ok(Ok(usage_under_160())));
    let raced = screen(&mut app);
    assert!(
        raced.contains("Usage: sessions 152/512 created · 360 left · active 3/4 · refreshing"),
        "{raced}"
    );
    policy_mut(&mut app).apply_usage(Ok(Ok(usage_after_512())));
    let fresh = screen(&mut app);
    assert!(
        fresh.contains("Usage: sessions 153/512 created · 359 left · active 3/4"),
        "{fresh}"
    );
}

/// #1046: the Actions journal row of a manager's daemon setting change shows
/// the key, the new and previous value, the receipt state and the reason.
#[test]
fn daemon_setting_action_row_shows_key_value_previous_and_reason() {
    let row = json!({
        "type": "action",
        "state": "succeeded",
        "operation": {
            "action": "operator_call",
            "call": {
                "method": "ProposeDaemonSetting",
                "params": {
                    "key": "sandbox_max_source_roots",
                    "value": 16384,
                    "reason": "wrapped to 512 with 1490 live roots"
                }
            }
        },
        "outcome": {
            "state": "succeeded",
            "operator_result": {
                "shape": "scalar",
                "method": "ProposeDaemonSetting",
                "result": {"key": "sandbox_max_source_roots", "previous": 512, "value": 16384,
                           "reason": "wrapped to 512 with 1490 live roots"}
            }
        }
    });
    assert_eq!(
        board::row_title(&row),
        "Daemon setting · sandbox_max_source_roots → 16384 · succeeded (was 512) · wrapped to 512 with 1490 live roots"
    );
}

fn menu_decision(key: &str, question: &str, version: i64, age_secs: i64) -> Value {
    let at = (chrono::Utc::now() - chrono::Duration::seconds(age_secs)).to_rfc3339();
    json!({"type":"decision","key":key,"row_version":version,"created_at":at,"updated_at":at,
        "payload":{"status":"pending","question":question,"target_digest":format!("sha256:{key}"),"epic_id":null}})
}

const MENU: &str = "Pick a mirror\nA) Direct\nB) API\nC) Mirror\nRecommend: C";

async fn open_decisions(
    app: &mut App,
    config: &HarnessManagerConfigV1,
    rows: Vec<Value>,
    extra: Vec<Case>,
) -> tokio::task::JoinHandle<Vec<Value>> {
    let mut cases = vec![(
        "GetHarnessManagerState",
        json!({"result":page(config, ManagerInspectSectionV2::Decisions, rows, None)}),
    )];
    cases.extend(extra);
    let (dir, task) = connect(app, cases).await;
    std::mem::forget(dir);
    board::open(app, config.clone(), "Coordination desk".into(), true)
        .await
        .unwrap();
    task
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn decisions_view_lists_live_first_and_shows_options_with_the_recommendation() {
    let (mut app, config) = fixture();
    let mut closed = menu_decision("closed-one", "Retire it?", 2, 500);
    closed["payload"]["status"] = json!("answered");
    closed["payload"]["answer"] = json!("A: yes");
    let rows = vec![
        closed,
        menu_decision("stale-one", "Old question?", 1, 20 * 86_400),
        menu_decision("census", MENU, 1, 7200),
    ];
    let task = open_decisions(&mut app, &config, rows, vec![]).await;
    finish(task).await;
    let text = screen(&mut app);
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {text}"))
    };
    assert!(at("NEEDS YOU 1") < at("STALE 1"));
    assert!(at("STALE 1") < at("CLOSED 1"));
    // The first live record is selected and shown whole.
    assert_eq!(board_state(&app).selected, 0);
    for value in [
        "Pick a mirror",
        "needs answer",
        "census",
        "OPTIONS",
        "Direct",
        "★ recommended",
        "HISTORY",
        "y accept ★ recommendation",
    ] {
        assert!(text.contains(value), "{value}: {text}");
    }
    assert_eq!(board_state(&app).effective_option(), Some(2));
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn decisions_enter_answers_the_highlighted_option_and_reloads() {
    let (mut app, config) = fixture();
    let reloaded = {
        let mut row = menu_decision("census", MENU, 2, 7200);
        row["payload"]["status"] = json!("answered");
        row["payload"]["answer"] = json!("B: API");
        vec![row]
    };
    let task = open_decisions(
        &mut app,
        &config,
        vec![menu_decision("census", MENU, 1, 7200)],
        vec![
            (
                "AnswerHarnessManagerDecision",
                json!({"result":{"state":"answered"}}),
            ),
            (
                "GetHarnessManagerState",
                json!({"result":page(&config, ManagerInspectSectionV2::Decisions, reloaded, None)}),
            ),
        ],
    )
    .await;
    // Starts on the recommendation (C); h moves to B.
    key(&mut app, KeyCode::Char('h')).await;
    assert_eq!(board_state(&app).effective_option(), Some(1));
    key(&mut app, KeyCode::Enter).await;
    let requests = finish(task).await;
    let params = &requests[1]["params"];
    assert_eq!(params["answer"], "B: API");
    assert_eq!(params["decision_key"], "census");
    assert_eq!(params["expected_row_version"], 1);
    assert_eq!(params["target_digest"], "sha256:census");
    let text = screen(&mut app);
    assert!(text.contains("CLOSED 1"), "{text}");
    assert!(text.contains("Answered census with “B: API”"), "{text}");
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn decisions_y_accepts_the_recommendation_and_refuses_without_one() {
    let (mut app, config) = fixture();
    let reloaded = page(&config, ManagerInspectSectionV2::Decisions, vec![], None);
    let task = open_decisions(
        &mut app,
        &config,
        vec![menu_decision("census", MENU, 1, 7200)],
        vec![
            (
                "AnswerHarnessManagerDecision",
                json!({"result":{"state":"answered"}}),
            ),
            ("GetHarnessManagerState", json!({"result":reloaded})),
        ],
    )
    .await;
    key(&mut app, KeyCode::Char('h')).await;
    key(&mut app, KeyCode::Char('y')).await;
    let requests = finish(task).await;
    // `y` answers the recommendation even with the cursor moved off it.
    assert_eq!(requests[1]["params"]["answer"], "C: Mirror");

    let (mut app, config) = fixture();
    let task = open_decisions(
        &mut app,
        &config,
        vec![menu_decision("plain", "Pick\nA) one\nB) two", 1, 60)],
        vec![],
    )
    .await;
    key(&mut app, KeyCode::Char('y')).await;
    finish(task).await;
    assert!(screen(&mut app).contains("no recommendation"));
    assert!(board_state(&app).answer.is_none());
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn decisions_enter_on_a_question_without_options_opens_the_answer_prompt() {
    let (mut app, config) = fixture();
    let task = open_decisions(
        &mut app,
        &config,
        vec![menu_decision("free", "Who is on call?", 1, 60)],
        vec![],
    )
    .await;
    key(&mut app, KeyCode::Enter).await;
    finish(task).await;
    assert!(board_state(&app).answer.is_some());
    assert!(screen(&mut app).contains("Answering free"));
}

#[test]
fn answer_refusals_are_explained_in_plain_words() {
    assert!(
        board::answer_error("rpc error: manager_v2_decision_changed")
            .contains("changed since the board loaded")
    );
    assert!(board::answer_error("manager_v2_explicit_grant_required").contains(":manager policy"));
    assert_eq!(board::answer_error("unmapped"), "unmapped");
}

fn stale_listing(question: &str, n: usize) -> Value {
    let rows: Vec<Value> = (0..n)
        .map(|i| {
            json!({"decision":{"project_id":Uuid::new_v4(),"owner_manager_session_id":Uuid::new_v4(),
                    "scope_version":3,"key":format!("old-{i}"),"expected_row_version":1},
                "epic_id":null,"question":question,"status":"pending","answerable_by":"manager",
                "gate":null,"created_at":"2026-09-01T00:00:00Z","age_seconds":3_000_000,
                "stale_reason":"older_than_days"})
        })
        .collect();
    json!({"older_than_days":7,"rows":rows,"complete":true})
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn decisions_show_structured_options_ruling_and_withdrawal() {
    let (mut app, config) = fixture();
    let mut ruled = menu_decision("ruled-one", "Mirror or API?", 2, 3600);
    ruled["status"] = json!("answered");
    ruled["answer"] = json!("A: Mirror");
    ruled["answered_by"] = json!({"kind":"portfolio_manager","session_id":Uuid::new_v4(),"node_label":"Global","at":"x"});
    ruled["answerable_by"] = json!(null);
    ruled["blocks"] = json!({"launches":false,"epic_ids":[]});
    let mut withdrawn = menu_decision("gone-one", "Old plan?", 2, 7200);
    withdrawn["status"] = json!("withdrawn");
    let mut live = menu_decision("live-one", "Pick a mirror?", 1, 600);
    live["answerable_by"] = json!("operator");
    live["gate"] = json!("spend");
    live["blocks"] = json!({"launches":true,"epic_ids":["abcdef12-0000"]});
    live["options"] = json!([
        {"label":"A","detail":"Read the DB","recommended":false},
        {"label":"B","detail":"Nightly mirror","recommended":true}]);
    let task = open_decisions(&mut app, &config, vec![ruled, withdrawn, live], vec![]).await;
    finish(task).await;
    let text = screen(&mut app);
    for value in [
        "Nightly mirror",
        "★ recommended",
        "Answerable by: you (operator) · a real gate: spend",
        "Blocks: create_session under Epic abcdef12",
        "ruled",
        "withdrawn",
    ] {
        assert!(text.contains(value), "{value}: {text}");
    }
    // The structured recommendation (B) is where Enter starts.
    assert_eq!(board_state(&app).effective_option(), Some(1));
    // Select the ruled record: it names the manager and shows no options cursor.
    key(&mut app, KeyCode::Char('j')).await;
    let text = screen(&mut app);
    for value in ["ruled by Global", "Ruled by: Global", "Mirror or API?"] {
        assert!(text.contains(value), "{value}: {text}");
    }
    key(&mut app, KeyCode::Char('j')).await;
    let text = screen(&mut app);
    assert!(text.contains("⊘ withdrawn"), "{text}");
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn stale_bulk_archive_confirms_the_count_then_archives_and_reloads() {
    let (mut app, config) = fixture();
    let reloaded = page(&config, ManagerInspectSectionV2::Decisions, vec![], None);
    let task = open_decisions(
        &mut app,
        &config,
        vec![menu_decision("old", "Old question?", 1, 20 * 86_400)],
        vec![
            (
                "ListStaleManagerDecisions",
                json!({"result":stale_listing("Retire the mirror?", 3)}),
            ),
            (
                "ArchiveStaleManagerDecisions",
                json!({"result":{"archived":stale_listing("q", 3)["rows"].as_array().unwrap()
                    .iter().map(|r| r["decision"].clone()).collect::<Vec<_>>(),"skipped":[]}}),
            ),
            ("GetHarnessManagerState", json!({"result":reloaded})),
        ],
    )
    .await;
    key(&mut app, KeyCode::Char('X')).await;
    let text = screen(&mut app);
    for value in [
        "Archive 3 stale decisions?",
        "Retire the mirror?",
        "never deleted",
    ] {
        assert!(text.contains(value), "{value}: {text}");
    }
    key(&mut app, KeyCode::Enter).await;
    let requests = finish(task).await;
    assert_eq!(
        requests[1]["params"]["project_id"],
        json!(config.project_id)
    );
    let items = requests[2]["params"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(requests[2]["params"]["older_than_days"], 7);
    assert!(board_state(&app).archive.is_none());
    assert!(screen(&mut app).contains("Archived 3 stale decisions"));
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn stale_bulk_archive_cancel_sends_nothing_and_empty_list_says_so() {
    let (mut app, config) = fixture();
    let task = open_decisions(
        &mut app,
        &config,
        vec![menu_decision("old", "Old question?", 1, 20 * 86_400)],
        vec![(
            "ListStaleManagerDecisions",
            json!({"result":stale_listing("Retire it?", 1)}),
        )],
    )
    .await;
    key(&mut app, KeyCode::Char('X')).await;
    assert!(screen(&mut app).contains("Archive 1 stale decision?"));
    key(&mut app, KeyCode::Esc).await;
    finish(task).await;
    assert!(board_state(&app).archive.is_none());
    assert!(screen(&mut app).contains("Nothing archived."));

    let (mut app, config) = fixture();
    let task = open_decisions(
        &mut app,
        &config,
        vec![],
        vec![(
            "ListStaleManagerDecisions",
            json!({"result":stale_listing("x", 0)}),
        )],
    )
    .await;
    key(&mut app, KeyCode::Char('X')).await;
    finish(task).await;
    assert!(screen(&mut app).contains("No stale decisions"));
}

fn set_editing_mode(app: &mut App, mode: &str) {
    crate::settings::DaemonFeatureEntry::update_from_json(
        &mut app.daemon_features,
        &json!({ "editing_mode": mode }),
    );
}

/// #1628 slice 3b: the policy editor's value gets a cursor and selection in
/// Standard editing; Vim mode keeps appending.
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn policy_value_is_edited_in_place_in_standard_mode_only() {
    for (mode, expected) in [("standard", "712"), ("vim", "127")] {
        let (mut app, config) = fixture();
        set_editing_mode(&mut app, mode);
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
        finish(task).await;
        policy_mut(&mut app).activate(policy::Field::Containers);
        policy_mut(&mut app).edit.as_mut().unwrap().1 = "12".to_string();
        key(&mut app, KeyCode::Home).await;
        key(&mut app, KeyCode::Char('7')).await;
        assert_eq!(
            policy_mut(&mut app).edit.as_ref().unwrap().1,
            expected,
            "{mode}"
        );
    }
}

/// #1628 slice 3b: the decision answer box gets a cursor in Standard editing
/// and typing refreshes the exact-target idempotency key.
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn decision_answer_is_edited_in_place_in_standard_mode() {
    let (mut app, config) = fixture();
    set_editing_mode(&mut app, "standard");
    let mut cases = board_cases(&config, board_overview_rows(&config), vec![], vec![]);
    cases[1].1 = json!({"result":page(
        &config,
        ManagerInspectSectionV2::Decisions,
        vec![
            pending_decision("first-gate", "Approve first gate?", 4),
            pending_decision("release-target", "Choose release target", 7),
        ],
        None,
    )});
    let (_dir, task) = connect(&mut app, cases).await;
    board::open(&mut app, config, "Coordination desk".into(), false)
        .await
        .unwrap();
    finish(task).await;
    key(&mut app, KeyCode::Char('j')).await;
    key(&mut app, KeyCode::Char('a')).await;
    for c in "lling".chars() {
        key(&mut app, KeyCode::Char(c)).await;
    }
    let before = board_state(&app)
        .answer
        .as_ref()
        .unwrap()
        .target
        .idempotency_key
        .clone();
    key(&mut app, KeyCode::Home).await;
    for c in "ro".chars() {
        key(&mut app, KeyCode::Char(c)).await;
    }
    let target = &board_state(&app).answer.as_ref().unwrap().target;
    assert_eq!(target.answer, "rolling");
    assert_ne!(target.idempotency_key, before, "editing refreshes the key");
}
