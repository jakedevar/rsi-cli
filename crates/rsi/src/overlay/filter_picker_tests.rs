//! Type-to-filter pickers treat every printable key as filter input in both
//! editing modes (Issue #3288). Navigation uses only non-printable keys.

use super::{handle_overlay_key, label_picker, memory_search, parent_picker, project_picker};
use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::types::{Project, SessionLabel};
use uuid::Uuid;

/// Contains every key that the old list navigation swallowed.
const TYPED: &str = "gjkG-gjk";

fn app_in(mode: &str) -> App {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    crate::settings::DaemonFeatureEntry::update_from_json(
        &mut app.daemon_features,
        &serde_json::json!({ "editing_mode": mode }),
    );
    app
}

fn project(name: &str) -> Project {
    Project {
        id: Uuid::new_v4(),
        name: name.into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn label(name: &str) -> SessionLabel {
    SessionLabel {
        id: Uuid::new_v4(),
        name: name.into(),
        description: None,
        project_id: None,
        color: "#cba6f7".into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

async fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    handle_overlay_key(app, KeyEvent::new(code, mods)).await;
}

async fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE).await;
    }
}

fn project_filter(app: &App) -> (String, usize) {
    match &app.overlay {
        OverlayState::ProjectPicker {
            filter,
            selected_index,
            ..
        } => (filter.clone(), *selected_index),
        _ => panic!("expected the project picker"),
    }
}

fn label_filter(app: &App) -> (String, usize) {
    match &app.overlay {
        OverlayState::LabelPicker {
            filter,
            selected_index,
        } => (filter.clone(), *selected_index),
        _ => panic!("expected the label picker"),
    }
}

#[tokio::test]
async fn project_picker_types_g_j_k_big_g_into_the_filter_in_both_modes() {
    for mode in ["vim", "standard"] {
        let mut app = app_in(mode);
        app.projects = vec![
            project("rsi-gateway"),
            project("jake-notes"),
            project("zzz"),
        ];
        project_picker::open_project_picker(&mut app, Default::default());
        type_text(&mut app, "gateway").await;
        let (filter, _) = project_filter(&app);
        assert_eq!(filter, "gateway", "{mode}");
        // "all projects" does not match, only rsi-gateway does.
        assert_eq!(
            project_picker::get_project_picker_filtered_count(&app, &filter, false),
            1,
            "{mode}"
        );

        let mut app = app_in(mode);
        app.projects = vec![project("rsi-gjkG"), project("other")];
        project_picker::open_project_picker(&mut app, Default::default());
        type_text(&mut app, "rsi-gjkG").await;
        let (filter, selected) = project_filter(&app);
        assert_eq!(filter, "rsi-gjkG", "{mode}");
        assert_eq!(selected, 0, "{mode}");
        assert_eq!(
            project_picker::get_project_picker_filtered_count(&app, &filter, false),
            1,
            "{mode}"
        );
        type_text(&mut app, TYPED).await;
        assert_eq!(project_filter(&app).0, format!("rsi-gjkG{TYPED}"), "{mode}");
    }
}

#[tokio::test]
async fn project_picker_navigates_with_down_and_ctrl_j_k() {
    let mut app = app_in("vim");
    app.projects = vec![project("a"), project("b"), project("c")];
    project_picker::open_project_picker(&mut app, Default::default());
    // "all projects" + 3 projects.
    press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
    assert_eq!(project_filter(&app).1, 1);
    press(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL).await;
    assert_eq!(project_filter(&app).1, 2);
    press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL).await;
    assert_eq!(project_filter(&app).1, 1);
    press(&mut app, KeyCode::End, KeyModifiers::NONE).await;
    assert_eq!(project_filter(&app).1, 3);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE).await;
    assert_eq!(project_filter(&app).1, 2);
    press(&mut app, KeyCode::Home, KeyModifiers::NONE).await;
    assert_eq!(project_filter(&app).1, 0);
    assert_eq!(project_filter(&app).0, "");
}

#[tokio::test]
async fn label_picker_types_g_j_k_big_g_into_the_filter_in_both_modes() {
    for mode in ["vim", "standard"] {
        let mut app = app_in(mode);
        app.labels = vec![label("bug-gjkG"), label("feature")];
        label_picker::open_label_picker(&mut app);
        type_text(&mut app, "bug-gjkG").await;
        let (filter, selected) = label_filter(&app);
        assert_eq!(filter, "bug-gjkG", "{mode}");
        assert_eq!(selected, 0, "{mode}");
        assert_eq!(
            label_picker::get_label_picker_filtered_count(&app, &filter),
            1,
            "{mode}"
        );
    }
}

#[tokio::test]
async fn label_picker_navigates_with_down_and_ctrl_j_k() {
    let mut app = app_in("vim");
    app.labels = vec![label("a"), label("b")];
    label_picker::open_label_picker(&mut app);
    // two labels + "(none)".
    press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
    assert_eq!(label_filter(&app).1, 1);
    press(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL).await;
    assert_eq!(label_filter(&app).1, 2);
    press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL).await;
    assert_eq!(label_filter(&app).1, 1);
    assert_eq!(label_filter(&app).0, "");
}

#[tokio::test]
async fn parent_picker_types_g_j_k_big_g_into_the_query_and_navigates_with_ctrl_j() {
    let mut app = app_in("vim");
    app.overlay = OverlayState::ParentPicker {
        session_id: Uuid::new_v4(),
        candidates: vec![Uuid::nil(), Uuid::nil(), Uuid::nil()],
        selected: 0,
        query: String::new(),
    };
    press(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL).await;
    press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
    match &app.overlay {
        OverlayState::ParentPicker { selected, .. } => assert_eq!(*selected, 2),
        _ => panic!("expected the parent picker"),
    }
    type_text(&mut app, "gjkG").await;
    match &app.overlay {
        OverlayState::ParentPicker {
            query, selected, ..
        } => {
            assert_eq!(query, "gjkG");
            assert_eq!(*selected, 0);
        }
        _ => panic!("expected the parent picker"),
    }
    let _ = parent_picker::filtered_indices(&app);
}

#[tokio::test]
async fn memory_search_types_q_j_k_g_into_the_query_and_esc_closes() {
    for mode in ["vim", "standard"] {
        let mut app = app_in(mode);
        memory_search::open_memory_search(&mut app);
        type_text(&mut app, "qjkgG").await;
        match &app.overlay {
            OverlayState::MemorySearch { query, .. } => assert_eq!(query, "qjkgG", "{mode}"),
            _ => panic!("memory search closed while typing in {mode} mode"),
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE).await;
        assert!(matches!(app.overlay, OverlayState::None), "{mode}");
    }
}
