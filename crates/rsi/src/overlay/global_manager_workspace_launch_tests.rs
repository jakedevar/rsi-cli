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

#[test]
fn the_host_defaults_to_the_active_project_and_cycles_through_the_scope() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, None, Some(f.b.id));
    assert!(form.fields().contains(&LaunchField::Host));
    assert_eq!(form.value(LaunchField::Host), "dictate-agent (default)");
    assert_eq!(form.validate(&projects).unwrap().home_project, f.b.id);

    form.field = LaunchField::Host;
    handle_form_key(&mut form, key(KeyCode::Right));
    assert_eq!(form.value(LaunchField::Host), "rsi");
    let request = form.validate(&projects).unwrap();
    assert_eq!(request.home_project, f.a.id, "the override is the host");
    assert_eq!(request.scope, vec![f.a.id, f.b.id], "scope is unchanged");
}

#[test]
fn the_host_falls_back_when_its_project_leaves_the_scope() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    // No active tab project: the first project in scope hosts.
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, None, None);
    assert_eq!(form.value(LaunchField::Host), "rsi (default)");
    form.field = LaunchField::Host;
    form.cycle(1);
    assert_eq!(form.host, Some(f.b.id));
    // Unchecking the chosen host drops the override.
    form.field = LaunchField::Scope;
    form.scope_cursor = 1;
    handle_form_key(&mut form, key(KeyCode::Char(' ')));
    assert_eq!(form.host, None);
    assert_eq!(form.validate(&projects).unwrap().home_project, f.a.id);
    // A PM seat has no host field: its project is the host.
    let pm = LaunchForm::new(LaunchRole::Project(f.b.id), &projects, None, None);
    assert!(!pm.fields().contains(&LaunchField::Host));
}

// === The grant editor in the form (#2237) ====================================

fn focus(form: &mut LaunchForm, field: LaunchField) {
    form.field = field;
}

#[test]
fn the_grant_field_opens_the_editor_and_only_then_is_the_grant_sent() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, None, None);
    assert!(form.fields().contains(&LaunchField::Grant));
    assert!(form.value(LaunchField::Grant).ends_with("(defaults)"));
    assert_eq!(
        form.validate(&projects).unwrap().grant,
        None,
        "an untouched grant keeps the old path: the seat's grant or the defaults"
    );

    focus(&mut form, LaunchField::Grant);
    assert_eq!(
        handle_form_key(&mut form, key(KeyCode::Enter)),
        FormOutcome::Stay,
        "Enter on Grant opens the editor instead of submitting"
    );
    assert!(form.grant_open && form.grant_visited);
    // The editor takes the keys: Space toggles the Preset row forward, and
    // Esc closes the editor instead of cancelling the form.
    handle_form_key(&mut form, key(KeyCode::Char('l')));
    assert_eq!(
        handle_form_key(&mut form, key(KeyCode::Esc)),
        FormOutcome::Stay
    );
    assert!(!form.grant_open);
    let choice = form
        .validate(&projects)
        .unwrap()
        .grant
        .expect("an edited grant");
    assert_eq!(
        choice.policy.capabilities.len(),
        15,
        "Execute -> Full project control"
    );
    assert_eq!(
        choice.launches,
        default_allowed_launches(),
        "the global grant keeps every default launch"
    );
    assert!(!form.value(LaunchField::Grant).contains("(defaults)"));
    // Esc with the editor closed cancels the form as before.
    assert_eq!(
        handle_form_key(&mut form, key(KeyCode::Esc)),
        FormOutcome::Cancel
    );
}

#[test]
fn the_editor_starts_from_the_grant_the_seat_holds() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut held = two_launch_grant(Uuid::new_v4(), &[f.a.id, f.b.id]);
    held.project_policy.max_created_sessions = 7;
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, Some(&held), None);
    focus(&mut form, LaunchField::Grant);
    handle_form_key(&mut form, key(KeyCode::Enter));
    let choice = form.grant_choice().expect("visited");
    assert_eq!(choice.policy.max_created_sessions, 7);
    assert_eq!(choice.launches, held.allowed_launches);

    // A revoked grant is not the starting point (#1902 prefills it elsewhere).
    let mut revoked = held.clone();
    revoked.state = "revoked".into();
    let form = LaunchForm::new(LaunchRole::Global, &projects, Some(&revoked), None);
    assert_eq!(form.grant.policy(), &default_project_policy());
}

#[test]
fn a_prefill_reseeds_the_editor_until_the_operator_has_opened_it() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, None, None);
    let mut prefill = default_project_policy();
    prefill.max_created_sessions = 11;
    let launches = vec![default_allowed_launches()[0].clone()];
    form.reseed_global_grant(prefill.clone(), launches.clone());
    assert_eq!(form.grant.policy().max_created_sessions, 11);
    assert_eq!(form.grant.launches(), launches.as_slice());
    // Once visited, the operator's draft is theirs.
    focus(&mut form, LaunchField::Grant);
    handle_form_key(&mut form, key(KeyCode::Enter));
    handle_form_key(&mut form, key(KeyCode::Esc));
    let mut other = default_project_policy();
    other.max_created_sessions = 2;
    form.reseed_global_grant(other, vec![]);
    assert_eq!(form.grant.policy().max_created_sessions, 11);
}

#[test]
fn a_project_managers_grant_is_its_own_policy_and_resets_with_the_seat() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut form = LaunchForm::new(LaunchRole::Project(f.a.id), &projects, None, None);
    assert!(form.fields().contains(&LaunchField::Grant));
    focus(&mut form, LaunchField::Grant);
    handle_form_key(&mut form, key(KeyCode::Right));
    assert!(form.grant_open, "l / Right on Grant opens the editor too");
    // Check the first launch: a PM's launches live in its policy.
    form.grant
        .select_row(crate::overlay::grant_editor::GrantRow::Launch(0));
    handle_form_key(&mut form, key(KeyCode::Char(' ')));
    handle_form_key(&mut form, key(KeyCode::Esc));
    let choice = form.validate(&projects).unwrap().grant.expect("edited");
    assert_eq!(
        choice.policy.allowed_launches,
        vec![default_allowed_launches()[0].clone()]
    );

    // Another seat starts from its own defaults: nothing carries over.
    focus(&mut form, LaunchField::Role);
    form.cycle(1);
    assert_eq!(form.role, LaunchRole::Project(f.b.id));
    assert!(!form.grant_visited);
    assert!(form.grant.finish().allowed_launches.is_empty());
    form.cycle(-1);
    form.cycle(-1);
    assert_eq!(form.role, LaunchRole::Global);
    assert_eq!(
        form.grant.launches().len(),
        default_allowed_launches().len()
    );
}

/// Cycle the Role field to `target`.
fn cycle_role_to(form: &mut LaunchForm, target: LaunchRole) {
    focus(form, LaunchField::Role);
    for _ in 0..form.roles.len() {
        if form.role == target {
            return;
        }
        handle_form_key(form, key(KeyCode::Char('l')));
    }
    assert_eq!(form.role, target, "the role is on offer");
}

#[test]
fn a_role_round_trip_returns_to_the_held_global_grant() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut held = two_launch_grant(Uuid::new_v4(), &[f.a.id, f.b.id]);
    held.project_policy.max_created_sessions = 7;
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, Some(&held), None);
    cycle_role_to(&mut form, LaunchRole::Project(f.a.id));
    assert_eq!(form.grant.policy(), &default_project_policy());
    cycle_role_to(&mut form, LaunchRole::Global);
    assert_eq!(form.grant.policy().max_created_sessions, 7);
    focus(&mut form, LaunchField::Grant);
    handle_form_key(&mut form, key(KeyCode::Enter));
    handle_form_key(&mut form, key(KeyCode::Esc));
    let choice = form.grant_choice().expect("visited");
    assert_eq!(choice.policy.max_created_sessions, 7);
    assert_eq!(choice.launches, held.allowed_launches);
}

#[test]
fn a_role_round_trip_returns_to_the_revoked_prefill() {
    let f = fixture();
    let projects = [f.a.clone(), f.b.clone()];
    let mut revoked = two_launch_grant(Uuid::new_v4(), &[f.a.id, f.b.id]);
    revoked.state = "revoked".into();
    let mut form = LaunchForm::new(LaunchRole::Global, &projects, Some(&revoked), None);
    let mut prefill = default_project_policy();
    prefill.max_created_sessions = 11;
    let launches = vec![default_allowed_launches()[0].clone()];
    form.reseed_global_grant(prefill, launches.clone());
    cycle_role_to(&mut form, LaunchRole::Project(f.a.id));
    cycle_role_to(&mut form, LaunchRole::Global);
    assert_eq!(form.grant.policy().max_created_sessions, 11);
    assert_eq!(form.grant.launches(), launches.as_slice());
    // Opening the editor and leaving with Esc keeps the same grant.
    focus(&mut form, LaunchField::Grant);
    handle_form_key(&mut form, key(KeyCode::Enter));
    handle_form_key(&mut form, key(KeyCode::Esc));
    let choice = form.grant_choice().expect("visited");
    assert_eq!(choice.policy.max_created_sessions, 11);
    assert_eq!(choice.launches, launches);
    // A visited editor still resets to the seed on a round trip.
    cycle_role_to(&mut form, LaunchRole::Project(f.a.id));
    cycle_role_to(&mut form, LaunchRole::Global);
    assert_eq!(form.grant.policy().max_created_sessions, 11);
    assert!(form.grant_choice().is_none(), "unvisited again");
}
