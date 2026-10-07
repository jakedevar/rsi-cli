//! Tests for the session-detail model/effort picker (Issue #681).

use super::*;
use crate::client::DaemonClient;
use crate::overlay::handle_overlay_key;
use crossterm::event::KeyModifiers;
use rsi_common::types::SessionProvider;
use std::path::PathBuf;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn options(
    session_id: Uuid,
    provider: SessionProvider,
    model: Option<&str>,
    effort: Option<&str>,
) -> SessionModelSwitchOptions {
    SessionModelSwitchOptions {
        session_id,
        provider,
        model: model.map(str::to_string),
        effort: effort.map(str::to_string),
        model_invocation_id: Some(Uuid::new_v4()),
        switchable: true,
        unavailable_reason: None,
        keeps_context: true,
        context_note: "Keeps the conversation; applies from the next turn".into(),
        model_allowlist: Vec::new(),
        pending: None,
    }
}

fn state_for(
    app: &App,
    provider: SessionProvider,
    model: &str,
    effort: Option<&str>,
) -> ModelSwitchState {
    let session_id = app.selected_session_id().expect("selected session");
    ModelSwitchState::new(app, options(session_id, provider, Some(model), effort))
        .expect("picker has models")
}

fn model_ids(state: &ModelSwitchState) -> Vec<&str> {
    state.models.iter().map(|(id, _)| id.as_str()).collect()
}

fn effort_names(state: &ModelSwitchState) -> Vec<Option<&str>> {
    state.efforts.iter().map(Option::as_deref).collect()
}

#[test]
fn picker_offers_the_provider_catalog_with_the_current_model_highlighted() {
    let app = crate::app::app_test_helpers::with_session_list(1);
    let catalog = crate::app::models_for_provider(SessionProvider::Claude);
    let current = catalog.last().expect("claude catalog").0.clone();

    let state = state_for(&app, SessionProvider::Claude, &current, None);

    assert_eq!(state.models, catalog);
    assert_eq!(state.models[state.model_index].0, current);
    assert_eq!(
        state.efforts[0], None,
        "the model default is always offered"
    );
}

#[test]
fn picker_narrows_models_to_the_operator_allowlist_and_keeps_the_highlight_on_an_allowed_one() {
    let app = crate::app::app_test_helpers::with_session_list(1);
    let catalog = crate::app::models_for_provider(SessionProvider::Claude);
    assert!(catalog.len() >= 2, "need two catalog models for this test");
    let allowed = catalog[1].0.clone();
    let session_id = app.selected_session_id().unwrap();
    let mut options = options(session_id, SessionProvider::Claude, Some(&allowed), None);
    options.model_allowlist = vec![allowed.clone()];

    let state = ModelSwitchState::new(&app, options).expect("one allowed model");

    assert_eq!(model_ids(&state), vec![allowed.as_str()]);
    assert_eq!(state.model_index, 0);
}

#[test]
fn picker_is_absent_when_the_allowlist_admits_no_model() {
    let app = crate::app::app_test_helpers::with_session_list(1);
    let session_id = app.selected_session_id().unwrap();
    let mut options = options(session_id, SessionProvider::Claude, None, None);
    options.model_allowlist = vec!["a-model-no-catalog-lists".into()];

    assert!(ModelSwitchState::new(&app, options).is_none());
}

#[test]
fn efforts_are_the_model_ladder_within_the_provider_switchable_levels() {
    let app = crate::app::app_test_helpers::with_session_list(1);
    let state = state_for(&app, SessionProvider::Claude, "claude-opus-5", Some("high"));

    let levels = effort_names(&state);
    let ladder = rsi_common::model_utils::effort_ladder("claude-opus-5");
    assert_eq!(levels[0], None);
    assert_eq!(
        levels[1..],
        ladder.iter().map(|level| Some(*level)).collect::<Vec<_>>()[..]
    );
    assert_eq!(state.chosen(), ("claude-opus-5", Some("high")));

    // Antigravity cannot switch to xhigh or max even when a ladder lists them.
    let allowed =
        rsi_common::model_utils::session_switch_effort_levels(SessionProvider::Antigravity)
            .expect("antigravity switches in place");
    let state = state_for(&app, SessionProvider::Antigravity, "gemini-3.6-flash", None);
    assert!(
        state
            .efforts
            .iter()
            .flatten()
            .all(|level| allowed.contains(&level.as_str()))
    );
}

#[tokio::test]
async fn moving_the_model_keeps_a_shared_effort_and_resets_an_unsupported_one() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let state = state_for(&app, SessionProvider::Claude, "claude-opus-5", Some("high"));
    let (first, last) = (
        state.models[0].0.clone(),
        state.models.last().unwrap().0.clone(),
    );
    app.overlay = OverlayState::ModelSwitch(Box::new(state));

    handle_overlay_key(&mut app, key(KeyCode::Char('g'))).await;
    let OverlayState::ModelSwitch(state) = &app.overlay else {
        panic!("picker stays open while navigating");
    };
    assert_eq!(state.chosen().0, first);
    let supports_high = rsi_common::model_utils::effort_ladder(&first).contains(&"high");
    assert_eq!(
        state.chosen().1,
        supports_high.then_some("high"),
        "a level the new model also offers survives the move; otherwise the default"
    );

    handle_overlay_key(&mut app, key(KeyCode::Char('G'))).await;
    let OverlayState::ModelSwitch(state) = &app.overlay else {
        panic!("picker stays open while navigating");
    };
    assert_eq!(state.chosen().0, last);
}

#[tokio::test]
async fn effort_keys_step_within_the_ladder_and_clamp_at_both_ends() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let state = state_for(&app, SessionProvider::Claude, "claude-opus-5", None);
    let count = state.efforts.len();
    assert!(count > 1);
    app.overlay = OverlayState::ModelSwitch(Box::new(state));

    handle_overlay_key(&mut app, key(KeyCode::Char('h'))).await;
    for _ in 0..count + 2 {
        handle_overlay_key(&mut app, key(KeyCode::Char('l'))).await;
    }
    let OverlayState::ModelSwitch(state) = &app.overlay else {
        panic!("picker open");
    };
    assert_eq!(state.effort_index, count - 1);
}

#[tokio::test]
async fn escape_closes_the_picker_without_queueing_anything() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let state = state_for(&app, SessionProvider::Claude, "claude-opus-5", None);
    app.overlay = OverlayState::ModelSwitch(Box::new(state));

    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;

    assert!(matches!(app.overlay, OverlayState::None));
    assert!(app.pending_model_switches.is_empty());
}

/// A one-connection fake daemon: answers each request in order with the next
/// canned result and returns every `(method, params)` it saw.
fn serve(
    socket_path: PathBuf,
    results: Vec<serde_json::Value>,
) -> tokio::task::JoinHandle<Vec<(String, serde_json::Value)>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::UnixListener::bind(&socket_path).expect("daemon listener");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client connects");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let mut seen = Vec::new();
        for result in results {
            let line = lines
                .next_line()
                .await
                .expect("request read")
                .expect("request line");
            let request: serde_json::Value = serde_json::from_str(&line).expect("request JSON");
            seen.push((
                request["method"].as_str().unwrap_or_default().to_string(),
                request["params"].clone(),
            ));
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": result,
            });
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .expect("response write");
        }
        seen
    })
}

#[tokio::test]
async fn opening_asks_the_daemon_and_enter_queues_the_chosen_switch() {
    let socket_dir = crate::test_support::short_socket_dir("rsi-model-switch");
    let socket_path = socket_dir.path().join("d.sock");
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let session_id = app.selected_session_id().expect("selected session");
    let fence = Uuid::new_v4();
    let mut daemon_options = options(
        session_id,
        SessionProvider::Claude,
        Some("claude-opus-5"),
        Some("high"),
    );
    daemon_options.model_invocation_id = Some(fence);
    let server = serve(
        socket_path.clone(),
        vec![
            serde_json::to_value(&daemon_options).expect("options JSON"),
            serde_json::json!({"state": "queued"}),
        ],
    );
    app.client = DaemonClient::new(socket_path);
    app.client.connect().await.expect("connect");

    open_model_switch_picker(&mut app).await;
    let OverlayState::ModelSwitch(state) = &app.overlay else {
        panic!(
            "picker opens for a switchable session: {:?}",
            app.notifications.back()
        );
    };
    assert_eq!(state.chosen(), ("claude-opus-5", Some("high")));

    handle_overlay_key(&mut app, key(KeyCode::Char('h'))).await;
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;

    assert!(matches!(app.overlay, OverlayState::None));
    let seen = tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("daemon served both requests")
        .expect("daemon task");
    assert_eq!(seen[0].0, "GetSessionModelSwitchOptions");
    assert_eq!(seen[0].1["session_id"], session_id.to_string());
    assert_eq!(seen[1].0, "QueueSessionModelUpdate");
    assert_eq!(seen[1].1["session_id"], session_id.to_string());
    assert_eq!(seen[1].1["expected_model_invocation_id"], fence.to_string());
    assert_eq!(seen[1].1["new_model"], "claude-opus-5");
    let lowered = rsi_common::model_utils::effort_ladder("claude-opus-5");
    let high = lowered.iter().position(|level| *level == "high").unwrap();
    let expected = if high == 0 {
        None
    } else {
        Some(lowered[high - 1])
    };
    assert_eq!(seen[1].1["new_effort"], serde_json::json!(expected));
    assert!(
        seen[1].1["idempotency_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
    );
    assert_eq!(
        app.pending_model_switches.get(&session_id),
        Some(&PendingSessionModelUpdate {
            model: "claude-opus-5".into(),
            effort: expected.map(str::to_string),
        })
    );
}

#[tokio::test]
async fn a_session_that_cannot_switch_gets_the_daemon_reason_and_no_picker() {
    let socket_dir = crate::test_support::short_socket_dir("rsi-model-switch");
    let socket_path = socket_dir.path().join("d.sock");
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let session_id = app.selected_session_id().expect("selected session");
    let mut unavailable = options(
        session_id,
        SessionProvider::Local,
        Some("local-model"),
        None,
    );
    unavailable.switchable = false;
    unavailable.keeps_context = false;
    unavailable.unavailable_reason =
        Some("model/effort switching is not supported for Local sessions".into());
    let server = serve(
        socket_path.clone(),
        vec![serde_json::to_value(&unavailable).expect("options JSON")],
    );
    app.client = DaemonClient::new(socket_path);
    app.client.connect().await.expect("connect");

    open_model_switch_picker(&mut app).await;

    assert!(matches!(app.overlay, OverlayState::None));
    assert_eq!(
        app.notifications.back().map(|n| n.message.as_str()),
        Some("model/effort switching is not supported for Local sessions")
    );
    server.await.expect("daemon task");
}

#[test]
fn picker_renders_current_queued_context_models_and_effort() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let session_id = app.selected_session_id().unwrap();
    let mut with_pending = options(
        session_id,
        SessionProvider::Claude,
        Some("claude-opus-5"),
        Some("high"),
    );
    with_pending.pending = Some(PendingSessionModelUpdate {
        model: "claude-sonnet-5".into(),
        effort: None,
    });
    let state = ModelSwitchState::new(&app, with_pending).expect("picker has models");
    let note = state.options.context_note.clone();
    app.overlay = OverlayState::ModelSwitch(Box::new(state));

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..30)
        .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    let screen = rows.join("\n");

    let current = format!("Current  {}", tuple_label("claude-opus-5", Some("high")));
    let queued = format!("Queued   {}", tuple_label("claude-sonnet-5", None));
    for expected in [
        "Switch Model / Effort",
        current.as_str(),
        queued.as_str(),
        note.as_str(),
        "[high]",
        "Enter: queue switch",
    ] {
        assert!(screen.contains(expected), "missing {expected:?}:\n{screen}");
    }
}
