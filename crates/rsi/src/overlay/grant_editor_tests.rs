use super::*;
use crate::overlay::global_manager_command::{default_allowed_launches, default_project_policy};
use rsi_common::harness_manager_presets::ManagerPolicyPreset;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(editor: &mut GrantEditor, ch: char) -> GrantKey {
    editor.handle_key(key(KeyCode::Char(ch)))
}

fn global() -> GrantEditor {
    GrantEditor::for_global(
        default_project_policy(),
        default_allowed_launches(),
        default_allowed_launches(),
    )
}

fn manager() -> GrantEditor {
    GrantEditor::for_manager(
        default_project_policy(),
        HarnessManagerScopeModeV1::Project,
        default_allowed_launches(),
    )
}

fn select(editor: &mut GrantEditor, row: GrantRow) {
    editor.select_row(row);
    assert_eq!(editor.selected_row(), row);
}

fn capability_index(capability: ManagerCapabilityV2) -> usize {
    CAPABILITIES
        .iter()
        .position(|candidate| *candidate == capability)
        .expect("capability is editable")
}

#[test]
fn the_global_grant_starts_from_the_execute_defaults_with_every_default_launch() {
    let editor = global();
    assert_eq!(editor.preset(), ManagerPolicyPreset::Execute);
    assert_eq!(editor.launches().len(), default_allowed_launches().len());
    assert_eq!(editor.policy(), &default_project_policy());
    assert!(editor.validate().is_ok());
    assert_eq!(
        editor.rows().len(),
        2 + CAPABILITIES.len() + 4 + default_allowed_launches().len()
    );
    assert!(
        editor.summary().starts_with("Execute · 8 capabilities"),
        "{}",
        editor.summary()
    );
}

#[test]
fn the_preset_row_cycles_observe_execute_full_and_resets_what_they_own() {
    let mut editor = global();
    select(&mut editor, GrantRow::Preset);
    // Execute -> Full project control.
    press(&mut editor, 'l');
    assert_eq!(editor.preset(), ManagerPolicyPreset::FullProjectControl);
    assert_eq!(editor.policy().capabilities.len(), CAPABILITIES.len());
    assert!(editor.policy().allow_create_groups);
    // Full -> Observe (wraps): watches, no capabilities, no creation allowance.
    press(&mut editor, 'l');
    assert_eq!(editor.preset(), ManagerPolicyPreset::Observe);
    assert!(editor.policy().capabilities.is_empty());
    assert_eq!(editor.policy().max_created_sessions, 0);
    assert!(!editor.policy().allow_create_groups);
    // Observe -> Execute with l, and h steps back the other way.
    press(&mut editor, 'l');
    assert_eq!(editor.preset(), ManagerPolicyPreset::Execute);
    press(&mut editor, 'h');
    assert_eq!(editor.preset(), ManagerPolicyPreset::Observe);
}

#[test]
fn toggling_capabilities_edits_the_draft_and_marks_it_custom() {
    let mut editor = global();
    let deploy = capability_index(ManagerCapabilityV2::Deploy);
    select(&mut editor, GrantRow::Capability(deploy));
    assert_eq!(editor.value(GrantRow::Capability(deploy)), "[ ]");
    assert_eq!(press(&mut editor, ' '), GrantKey::Handled);
    assert_eq!(editor.value(GrantRow::Capability(deploy)), "[x]");
    assert!(
        editor
            .policy()
            .capabilities
            .contains(&ManagerCapabilityV2::Deploy)
    );
    assert_eq!(editor.preset(), ManagerPolicyPreset::Custom);
    press(&mut editor, ' ');
    assert_eq!(editor.preset(), ManagerPolicyPreset::Execute);
}

#[test]
fn dropping_topology_also_drops_group_creation_the_daemon_would_refuse() {
    let mut editor = global();
    select(&mut editor, GrantRow::Preset);
    press(&mut editor, 'l'); // Full: allow_create_groups on
    assert!(editor.policy().allow_create_groups);
    let topology = capability_index(ManagerCapabilityV2::Topology);
    select(&mut editor, GrantRow::Capability(topology));
    press(&mut editor, ' ');
    assert!(!editor.policy().allow_create_groups);
    assert!(editor.validate().is_ok());
}

#[test]
fn numbers_type_in_place_and_stay_inside_the_daemon_bounds() {
    let mut editor = global();
    select(&mut editor, GrantRow::Sessions);
    for ch in "24".chars() {
        press(&mut editor, ch);
    }
    assert_eq!(editor.policy().max_created_sessions, 24);
    assert_eq!(editor.typed(), "24");
    // 1025 is past the bound: the keystroke is refused and the value stays.
    select(&mut editor, GrantRow::Sessions);
    for ch in "1025".chars() {
        press(&mut editor, ch);
    }
    assert_eq!(editor.policy().max_created_sessions, 102);
    assert_eq!(
        editor.error.as_deref(),
        Some("Sessions must be 0-1024"),
        "the refusal names the bound"
    );
    // Active concurrency never reaches 0.
    select(&mut editor, GrantRow::Active);
    press(&mut editor, '0');
    assert_eq!(
        editor.policy().max_active_sessions,
        4,
        "0 is below the minimum"
    );
    press(&mut editor, '9');
    assert_eq!(editor.policy().max_active_sessions, 9);
    editor.handle_key(key(KeyCode::Backspace));
    assert_eq!(editor.policy().max_active_sessions, 1);
    // h/l step the value.
    press(&mut editor, 'l');
    assert_eq!(editor.policy().max_active_sessions, 2);
    select(&mut editor, GrantRow::Containers);
    press(&mut editor, '0');
    press(&mut editor, 'h');
    assert_eq!(editor.policy().max_created_containers, 0, "clamped at 0");
}

#[test]
fn the_spend_ceiling_steps_from_uncapped_and_back() {
    let mut editor = global();
    select(&mut editor, GrantRow::Spend);
    assert_eq!(editor.value(GrantRow::Spend), "uncapped");
    press(&mut editor, 'l');
    assert_eq!(editor.value(GrantRow::Spend), "$5");
    press(&mut editor, 'l');
    assert_eq!(editor.value(GrantRow::Spend), "$10");
    press(&mut editor, 'h');
    press(&mut editor, 'h');
    assert_eq!(editor.value(GrantRow::Spend), "uncapped");
    for ch in "12.5".chars() {
        press(&mut editor, ch);
    }
    assert_eq!(editor.policy().max_spend_usd, Some(12.5));
    assert!(editor.validate().is_ok());
}

#[test]
fn a_global_grant_needs_a_launch_and_names_the_row_to_fix() {
    let mut editor = global();
    let count = default_allowed_launches().len();
    for index in 0..count {
        select(&mut editor, GrantRow::Launch(index));
        press(&mut editor, ' ');
    }
    assert!(editor.launches().is_empty());
    let (row, message) = editor.validate().unwrap_err();
    assert_eq!(row, GrantRow::Launch(0));
    assert!(message.contains("at least one launch"), "{message}");
    select(&mut editor, GrantRow::Launch(2));
    press(&mut editor, ' ');
    assert_eq!(editor.launches(), &[default_allowed_launches()[2].clone()]);
    assert!(editor.validate().is_ok());
}

#[test]
fn a_managers_launches_fold_into_its_policy_but_the_globals_stay_beside_it() {
    let mut manager = manager();
    assert!(
        manager.finish().allowed_launches.is_empty(),
        "a new PM starts with none"
    );
    select(&mut manager, GrantRow::Launch(0));
    press(&mut manager, ' ');
    assert_eq!(
        manager.finish().allowed_launches,
        vec![default_allowed_launches()[0].clone()]
    );
    assert!(manager.validate().is_ok(), "a PM may hold no launches");

    let global = global();
    assert!(global.finish().allowed_launches.is_empty());
    assert_eq!(global.launches().len(), default_allowed_launches().len());
}

#[test]
fn warnings_flag_a_grant_that_cannot_work_without_blocking_it() {
    let mut editor = global();
    select(&mut editor, GrantRow::Sessions);
    press(&mut editor, '0');
    let warnings = editor.warnings();
    assert!(
        warnings.iter().any(|line| line.contains("0 allowance")),
        "{warnings:?}"
    );
    assert!(editor.validate().is_ok());
}

#[test]
fn escape_leaves_and_unrelated_keys_are_left_to_the_caller() {
    let mut editor = global();
    assert_eq!(editor.handle_key(key(KeyCode::Esc)), GrantKey::Leave);
    assert_eq!(press(&mut editor, 'z'), GrantKey::Ignored);
    assert_eq!(
        editor.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
        GrantKey::Ignored
    );
}

#[test]
fn no_key_sequence_builds_a_grant_the_daemon_would_refuse() {
    // A deterministic walk over every editing key: the draft is valid by
    // construction after each one, so a submit never meets a daemon refusal.
    let keys = ['j', 'k', 'h', 'l', ' ', '0', '1', '5', '9', '.', 'g', 'G'];
    let mut seed = 0x2237_u64;
    for mut editor in [global(), manager()] {
        for _ in 0..4000 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let pick = (seed >> 33) as usize % (keys.len() + 1);
            if pick == keys.len() {
                editor.handle_key(key(KeyCode::Backspace));
            } else {
                press(&mut editor, keys[pick]);
            }
            let policy = editor.finish();
            assert_eq!(policy.validate(), Ok(()), "after {pick}: {policy:?}");
        }
    }
}

fn type_spend(text: &str) -> GrantEditor {
    let mut editor = global();
    select(&mut editor, GrantRow::Spend);
    for ch in text.chars() {
        press(&mut editor, ch);
    }
    editor
}

#[test]
fn typing_half_a_dollar_submits_fifty_cents() {
    for text in ["0.50", ".5", "0.5"] {
        let editor = type_spend(text);
        assert_eq!(
            editor.finish().max_spend_usd,
            Some(0.5),
            "{text} is fifty cents"
        );
        assert_eq!(editor.value(GrantRow::Spend), format!("${text}"));
        assert_eq!(editor.error, None, "{text} raises no error");
    }
}

#[test]
fn decimal_prefixes_wait_in_the_edit_buffer_until_a_value_is_complete() {
    let mut editor = type_spend("0");
    assert_eq!(editor.typed(), "0");
    assert_eq!(
        editor.policy().max_spend_usd,
        default_project_policy().max_spend_usd
    );
    press(&mut editor, '.');
    assert_eq!(editor.typed(), "0.");
    assert_eq!(editor.value(GrantRow::Spend), "$0.");
    press(&mut editor, '2');
    press(&mut editor, '5');
    assert_eq!(editor.finish().max_spend_usd, Some(0.25));
    // A third decimal and a second point are refused.
    press(&mut editor, '9');
    press(&mut editor, '.');
    assert_eq!(editor.typed(), "0.25");
    // Moving off the row shows the value with its cents.
    select(&mut editor, GrantRow::Active);
    assert_eq!(editor.value(GrantRow::Spend), "$0.25");
}

#[test]
fn whole_dollar_spend_still_types_and_shows_bare() {
    let editor = type_spend("12");
    assert_eq!(editor.finish().max_spend_usd, Some(12.0));
    let mut editor = editor;
    select(&mut editor, GrantRow::Active);
    assert_eq!(editor.value(GrantRow::Spend), "$12");
}
