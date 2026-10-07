//! Tests for the instantiate-a-manager form (#1231).

use super::*;
use crate::overlay::global_manager_workspace::tests::{fixture, grant, project};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn two_launch_grant(seat: Uuid, projects: &[Uuid]) -> GlobalManagerGrantV1 {
    let mut grant = grant(seat, projects, "active");
    grant.allowed_launches = vec![
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Codex,
            model: "gpt-6-astra".into(),
            effort: Some("xhigh".into()),
        },
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Codex,
            model: "gpt-6-astra".into(),
            effort: Some("high".into()),
        },
    ];
    grant
}

#[test]
fn the_catalog_is_the_grants_launches_plus_the_operator_default() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let grant = two_launch_grant(f.seat_id, &[f.a.id]);
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, Some(&grant), None);
    assert_eq!(&form.catalog[..2], &grant.allowed_launches);
    // Both grant entries are already operator defaults. Every current
    // default is offered once, even as the directive adds worker models.
    let defaults = default_allowed_launches();
    assert_eq!(form.catalog.len(), defaults.len());
    for launch in defaults {
        assert!(form.catalog.contains(&launch));
    }
    assert_eq!(form.value(LaunchField::Provider), "Codex");
    assert_eq!(form.value(LaunchField::Effort), "xhigh");
    // Scope defaults to the grant's projects.
    assert_eq!(
        form.scope.iter().map(|e| e.checked).collect::<Vec<_>>(),
        vec![true, false]
    );
    form.field = LaunchField::Effort;
    form.cycle(1);
    assert_eq!(form.value(LaunchField::Effort), "high");
    form.field = LaunchField::Provider;
    form.cycle(1);
    assert_eq!(form.value(LaunchField::Provider), "Claude");
    assert_eq!(form.value(LaunchField::Model), "claude-opus-5-5");
    assert_eq!(form.value(LaunchField::Effort), "high");
    assert!(form.catalog.contains(&form.launch));
}

#[test]
fn without_a_grant_every_project_is_in_scope_and_the_default_launch_is_offered() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let form = LaunchForm::new(LaunchRole::Global, &projects, None, Some(f.b.id));
    assert!(form.scope.iter().all(|e| e.checked));
    assert_eq!(form.catalog, default_allowed_launches());
    let request = form.validate(&projects).unwrap();
    assert_eq!(request.scope, vec![f.a.id, f.b.id]);
    assert_eq!(
        request.home_project, f.b.id,
        "the tab's project when in scope"
    );
    assert!(request.prompt.contains("rsi, dictate-agent"));
}

#[test]
fn the_role_cycles_through_the_global_seat_and_each_projects_pm() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, None, None);
    assert!(form.fields().contains(&LaunchField::Scope));
    form.cycle(1);
    assert_eq!(form.role, LaunchRole::Project(f.a.id));
    assert!(!form.fields().contains(&LaunchField::Scope));
    assert!(form.prompt.contains("project manager for rsi"));
    form.cycle(1);
    form.cycle(1);
    assert_eq!(form.role, LaunchRole::Global, "wraps");
    let request = LaunchForm::new(LaunchRole::Project(f.b.id), &projects, None, None)
        .validate(&projects)
        .unwrap();
    assert_eq!(request.home_project, f.b.id);
    assert!(request.scope.is_empty());
    assert_eq!(request.title, "Project manager · dictate-agent");
}

#[test]
fn validation_names_the_field_to_fix() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, None, None);
    form.field = LaunchField::Prompt;
    handle_form_key(
        &mut form,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    assert!(form.validate(&projects).unwrap_err().contains("Prompt"));
    for ch in "go".chars() {
        handle_form_key(&mut form, key(KeyCode::Char(ch)));
    }
    assert_eq!(form.prompt, "go");
    assert!(form.prompt_edited);
    form.field = LaunchField::Scope;
    handle_form_key(&mut form, key(KeyCode::Char('a')));
    assert!(form.validate(&projects).unwrap_err().contains("Scope"));
    handle_form_key(&mut form, key(KeyCode::Char(' ')));
    let request = form.validate(&projects).unwrap();
    assert_eq!(request.scope, vec![f.a.id]);
    assert_eq!(request.prompt, "go", "an edited prompt is kept");

    // A project deleted since the form opened.
    let gone = project("gone");
    let mut stale = LaunchForm::new(LaunchRole::Project(gone.id), &[gone.clone()], None, None);
    stale.prompt = "x".into();
    assert!(
        stale
            .validate(&projects)
            .unwrap_err()
            .contains("no longer exists")
    );
}

#[test]
fn form_keys_move_fields_and_report_submit_and_cancel() {
    let f = fixture();
    let projects = [f.a.clone()];
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, None, None);
    assert_eq!(form.field, LaunchField::Role);
    handle_form_key(&mut form, key(KeyCode::Tab));
    assert_eq!(form.field, LaunchField::Provider);
    handle_form_key(&mut form, key(KeyCode::BackTab));
    handle_form_key(&mut form, key(KeyCode::BackTab));
    assert_eq!(form.field, LaunchField::Submit, "wraps");
    assert_eq!(
        handle_form_key(&mut form, key(KeyCode::Enter)),
        FormOutcome::Submit
    );
    assert_eq!(
        handle_form_key(&mut form, key(KeyCode::Esc)),
        FormOutcome::Cancel
    );
    // Once launched, the seat is fixed.
    form.launched = Some(Uuid::new_v4());
    form.field = LaunchField::Role;
    form.cycle(1);
    assert_eq!(form.role, LaunchRole::Global);
    assert!(form.error.as_deref().unwrap().contains("already launched"));
}
