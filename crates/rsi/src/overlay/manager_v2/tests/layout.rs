//! #3418: the redesigned Policy screen: one summary block, a grants checklist,
//! one-line presets, collapsed provider limits and allowed models grouped by
//! provider. Snapshots use the operator's screenshot state: Full project
//! control, scope 93 / policy 107, 183 of 1024 sessions created, 17 allowed
//! launches and no provider limit.
use super::*;
use rsi_common::{
    harness_manager::HarnessManagerScopeModeV1, harness_manager_presets::*, types::SessionProvider,
};
use std::collections::BTreeSet;

fn choice(provider: SessionProvider, model: &str, effort: Option<&str>) -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider,
        model: model.into(),
        effort: effort.map(str::to_string),
    }
}

/// Seventeen tuples, stored out of provider order so the grouping shows.
fn seventeen() -> Vec<ManagerLaunchChoiceV2> {
    use SessionProvider::*;
    vec![
        choice(Codex, "gpt-6-astra", Some("high")),
        choice(Claude, "claude-opus-5", Some("xhigh")),
        choice(Claude, "claude-sonnet-5-5", Some("high")),
        choice(Claude, "claude-haiku-5", None),
        choice(Codex, "gpt-6-astra", Some("low")),
        choice(Pioneer, "pioneer-large", Some("high")),
        choice(Claude, "claude-opus-5", Some("high")),
        choice(Codex, "gpt-6-astra", Some("xhigh")),
        choice(OpenRouter, "vendor/model-a", None),
        choice(Claude, "claude-sonnet-5-5", Some("medium")),
        choice(Codex, "gpt-6-mini", Some("high")),
        choice(Local, "local-coder", None),
        choice(Pioneer, "pioneer-small", None),
        choice(OpenRouter, "vendor/model-b", Some("high")),
        choice(Antigravity, "ag-pro", None),
        choice(Harness, "harness-direct", Some("high")),
        choice(Codex, "gpt-6-mini", Some("low")),
    ]
}

fn screenshot_policy(config: &HarnessManagerConfigV1) -> HarnessManagerPolicyConfigV2 {
    let mut saved = policy_config(config);
    saved.scope_version = 93;
    saved.row_version = 107;
    saved.policy = apply_manager_policy_preset(
        &ManagerPolicyV2::default(),
        ManagerPolicyPreset::FullProjectControl,
        &ManagerPresetContext {
            origin: ManagerPolicyOrigin::New,
            scope_mode: HarnessManagerScopeModeV1::Project,
            touched: &BTreeSet::new(),
        },
    )
    .policy;
    saved.policy.max_created_sessions = 1024;
    saved.policy.max_created_containers = 64;
    saved.policy.max_recovery_attempts = 3;
    saved.policy.allowed_launches = seventeen();
    saved
}

/// The screenshot's Policy screen, opened for real.
async fn screenshot_app() -> App {
    let (mut app, mut config) = fixture();
    config.scope_mode = HarnessManagerScopeModeV1::Project;
    config.row_version = 93;
    config.epic_ids.clear();
    let (_dir, task) = connect(
        &mut app,
        vec![(
            "GetHarnessManagerPolicy",
            json!({"result":screenshot_policy(&config)}),
        )],
    )
    .await;
    policy::open(&mut app, config, "Coordination desk".into())
        .await
        .unwrap();
    finish(task).await;
    let state = policy_mut(&mut app);
    state.preview_open = false;
    state.apply_usage(Ok(Ok(policy::usage::CreationUsage {
        created: 183,
        created_limit: Some(1024),
        created_remaining: Some(841),
        window_hours: None,
        next_free_at: None,
        lifetime_total: None,
        active: 3,
        grants: vec![],
        bench_enabled: None,
    })));
    app
}

fn screen_at(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn screenshot_state_layout_at_120_columns() {
    let mut app = screenshot_app().await;
    insta::assert_snapshot!("policy_screenshot_state_120", screen_at(&mut app, 120, 100));
}

#[tokio::test]
async fn screenshot_state_layout_at_200_columns() {
    let mut app = screenshot_app().await;
    insta::assert_snapshot!("policy_screenshot_state_200", screen_at(&mut app, 200, 100));
}

#[tokio::test]
async fn screenshot_state_layout_narrow_fallback_at_72_columns() {
    let mut app = screenshot_app().await;
    insta::assert_snapshot!("policy_screenshot_state_72", screen_at(&mut app, 72, 100));
}

#[tokio::test]
async fn summary_block_states_mode_preset_quota_spend_and_model_count_once() {
    let mut app = screenshot_app().await;
    let state = policy_mut(&mut app);
    assert_eq!(
        state.mode_summary(),
        "Mode Execute · Preset Full project control"
    );
    assert_eq!(
        state.usage_summary(),
        "Usage: sessions 183/1024 created (lifetime) · 841 left · active 3/4"
    );
    assert_eq!(
        state.limits_summary(),
        "Spend uncapped · Allowed models: 17 (Claude 5 · Codex 5 · Pioneer 2 · OpenRouter 2 · Local 1 · Antigravity 1 · Harness 1)"
    );
    assert_eq!(
        state.saved_line(),
        "Saved: Full project control · scope 93 / policy 107 · draft unchanged"
    );
    assert_eq!(state.draft_line(), None);
}

#[tokio::test]
async fn draft_line_appears_only_while_the_draft_differs_and_names_each_change() {
    let mut app = screenshot_app().await;
    let state = policy_mut(&mut app);
    state
        .apply_text(policy::Field::Sessions, "2048")
        .unwrap_err();
    state.apply_text(policy::Field::Sessions, "512").unwrap();
    state.activate(policy::Field::Capability(14));
    state.activate(policy::Field::Mode);
    state.activate(policy::Field::RemoveLaunch(0));
    let line = state.draft_line().unwrap();
    assert!(line.starts_with("Draft: Custom"), "{line}");
    for part in [
        "Created session quota 1024→512",
        "Operating mode Execute→Status",
        "grants −Deploy",
        "allowed models +0 −1",
    ] {
        assert!(line.contains(part), "{part}: {line}");
    }
    assert!(!state.saved_line().contains("draft unchanged"));
    // Undoing every change makes the diff line go away again.
    state.draft = state.opened_draft.clone();
    assert_eq!(state.draft_line(), None);
}

#[tokio::test]
async fn presets_are_single_lines_that_each_say_what_they_change() {
    let mut app = screenshot_app().await;
    let rows = policy_mut(&mut app).rows();
    let value = |preset| {
        rows.iter()
            .find(|r| r.field == policy::Field::Preset(preset))
            .unwrap()
            .value
            .clone()
    };
    let observe = value(ManagerPolicyPreset::Observe);
    let execute = value(ManagerPolicyPreset::Execute);
    let full = value(ManagerPolicyPreset::FullProjectControl);
    let custom = value(ManagerPolicyPreset::Custom);
    assert_eq!(
        observe,
        "Monitor mode · no write grants · watches and reports only"
    );
    assert_eq!(
        execute,
        "Execute mode · 8 of 15 grants · withholds Topology, GitEffect +5 more"
    );
    let help = |preset| {
        rows.iter()
            .find(|r| r.field == policy::Field::Preset(preset))
            .unwrap()
            .description
            .clone()
    };
    assert!(
        help(ManagerPolicyPreset::Execute).ends_with(
            "withholds Topology, GitEffect, Operator delegation, Automation, Storage control, Daemon settings, Deploy"
        ),
        "{}",
        help(ManagerPolicyPreset::Execute)
    );
    assert_eq!(
        full,
        "Execute mode · all 15 grants, including Git, Topology, Automation and Deploy"
    );
    assert!(
        custom.starts_with("Keep the draft exactly as edited"),
        "{custom}"
    );
    let all = [&observe, &execute, &full, &custom];
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), 4);
    assert!(all.iter().all(|v| !v.contains('\n')));
}

#[tokio::test]
async fn grants_are_one_checklist_with_a_meaning_each_toggled_with_space() {
    let mut app = screenshot_app().await;
    let grants = policy_mut(&mut app)
        .rows()
        .into_iter()
        .filter(|r| matches!(r.field, policy::Field::Capability(_)))
        .collect::<Vec<_>>();
    assert_eq!(grants.len(), 15);
    assert!(grants.iter().all(|r| r.check == Some(true)));
    assert!(grants.iter().all(|r| !r.description.is_empty()));
    assert!(grants.iter().all(|r| !r.label.starts_with("Grant ")));
    let wide = flat(&screen_at(&mut app, 140, 100));
    assert!(
        wide.contains("[x] WorkPlan Record work, stages, evidence"),
        "{wide}"
    );
    assert!(
        wide.contains("[x] Deploy Ask the daemon to stage built binaries"),
        "{wide}"
    );

    focus_field(&mut app, policy::Field::Capability(14));
    key(&mut app, KeyCode::Char(' ')).await;
    let state = policy_mut(&mut app);
    assert!(
        !state
            .draft
            .capabilities
            .contains(&ManagerCapabilityV2::Deploy)
    );
    let deploy = state
        .rows()
        .into_iter()
        .find(|r| r.field == policy::Field::Capability(14))
        .unwrap();
    assert_eq!(deploy.check, Some(false));
    assert!(deploy.modified);
    assert!(
        flat(&screen_at(&mut app, 140, 100)).contains("[ ] Deploy (saved on)"),
        "unchecked grant names the saved state"
    );
}

#[tokio::test]
async fn narrow_pane_keeps_the_checklist_and_shows_the_meaning_in_the_detail_line() {
    let mut app = screenshot_app().await;
    focus_field(&mut app, policy::Field::Capability(0));
    let narrow = flat(&screen_at(&mut app, 72, 100));
    assert!(narrow.contains("[x] WorkPlan"), "{narrow}");
    assert!(
        narrow.contains("Enter toggles · Record work, stages, evidence"),
        "{narrow}"
    );
}

#[tokio::test]
async fn provider_limits_collapse_to_none_set_until_expanded() {
    let mut app = screenshot_app().await;
    let state = policy_mut(&mut app);
    let listed = |state: &policy::PolicyState| {
        state
            .rows()
            .iter()
            .filter(|r| matches!(r.field, policy::Field::ProviderLimit(_)))
            .count()
    };
    assert_eq!(listed(state), 0);
    let row = state
        .rows()
        .into_iter()
        .find(|r| r.field == policy::Field::ProviderLimits)
        .unwrap();
    assert_eq!(row.value, "none set · Enter to set one");
    state.activate(policy::Field::ProviderLimits);
    assert_eq!(listed(state), policy::PROVIDERS.len());
    // A limit that is set stays listed once the section is collapsed again.
    state
        .apply_text(policy::Field::ProviderLimit(1), "2")
        .unwrap();
    state.activate(policy::Field::ProviderLimits);
    assert_eq!(listed(state), 1);
    let row = state
        .rows()
        .into_iter()
        .find(|r| r.field == policy::Field::ProviderLimits)
        .unwrap();
    assert_eq!(row.value, "Codex 2 · Enter to show every provider");
    let text = flat(&screen_at(&mut app, 120, 100));
    assert!(
        text.contains("Codex active limit 2 (saved inherit)"),
        "{text}"
    );
}

#[tokio::test]
async fn allowed_models_list_matches_the_stored_launches_grouped_by_provider() {
    let mut app = screenshot_app().await;
    let stored = seventeen();
    let state = policy_mut(&mut app);
    assert_eq!(state.draft.allowed_launches, stored);
    let rows = state.rows();
    let listed = rows
        .iter()
        .filter_map(|r| match r.field {
            policy::Field::EditLaunch(i) => Some((i, r)),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Every stored launch is listed exactly once, under its own provider.
    let mut indices = listed.iter().map(|(i, _)| *i).collect::<Vec<_>>();
    indices.sort_unstable();
    assert_eq!(indices, (0..stored.len()).collect::<Vec<_>>());
    for (i, row) in &listed {
        let c = &stored[*i];
        assert_eq!(row.label, c.model);
        assert!(row.value.starts_with(c.effort_label()), "{}", row.value);
        let group = row.group.as_deref().unwrap();
        assert!(
            group.starts_with(&format!("{:?} · ", c.provider)),
            "{group}"
        );
    }
    // Providers appear in provider order, each as one contiguous run.
    let order = listed.iter().map(|(_, r)| r.group.clone().unwrap()).fold(
        Vec::<String>::new(),
        |mut acc, g| {
            if acc.last() != Some(&g) {
                acc.push(g);
            }
            acc
        },
    );
    assert_eq!(
        order,
        [
            "Claude · 5",
            "Codex · 5",
            "Pioneer · 2",
            "OpenRouter · 2",
            "Local · 1",
            "Antigravity · 1",
            "Harness · 1"
        ]
    );
    let summary = rows
        .iter()
        .find(|r| r.field == policy::Field::AddLaunch)
        .unwrap();
    assert!(summary.value.starts_with("Restricted to 17 exact choices"));
    let text = flat(&screen_at(&mut app, 120, 100).replace('│', " "));
    assert!(text.contains("Claude · 5 claude-opus-5 xhigh"), "{text}");
}

#[tokio::test]
async fn x_removes_the_focused_launch_and_keeps_save_and_discard_semantics() {
    let mut app = screenshot_app().await;
    let stored = seventeen();
    let target = stored
        .iter()
        .position(|c| c.model == "ag-pro")
        .expect("fixture has the Antigravity launch");
    focus_field(&mut app, policy::Field::EditLaunch(target));
    key(&mut app, KeyCode::Char('x')).await;
    let state = policy_mut(&mut app);
    assert_eq!(state.draft.allowed_launches.len(), 16);
    assert!(
        state
            .draft
            .allowed_launches
            .iter()
            .all(|c| c.model != "ag-pro")
    );
    assert_eq!(state.opened_draft.allowed_launches, stored);
    assert!(state.selected < state.rows().len());
    assert!(
        state.draft_line().unwrap().contains("allowed models +0 −1"),
        "{:?}",
        state.draft_line()
    );
    // x anywhere else changes nothing.
    focus_field(&mut app, policy::Field::Capability(0));
    key(&mut app, KeyCode::Char('x')).await;
    assert_eq!(policy_mut(&mut app).draft.allowed_launches.len(), 16);
}

/// #3416 on the #3418 layout: with a daemon-reported window the summary block
/// carries used, remaining, next refill and the lifetime total, and the
/// "Created session window (hours)" row is part of the budgets section.
#[tokio::test]
async fn windowed_usage_header_and_window_row_appear_in_the_redesigned_layout() {
    let mut app = screenshot_app().await;
    policy_mut(&mut app).apply_usage(Ok(Ok(policy::usage::CreationUsage {
        created: 183,
        created_limit: Some(1024),
        created_remaining: Some(841),
        window_hours: Some(24),
        next_free_at: Some("2026-10-21T04:12:00.000000000Z".into()),
        lifetime_total: Some(900),
        active: 3,
        grants: vec![],
        bench_enabled: None,
    })));
    let expected = "Usage: sessions 183 of 1024 created in the last 24 h; next slot frees at 2026-10-21 04:12 UTC · 841 left · 900 ever · active 3/4";
    assert_eq!(policy_mut(&mut app).usage_summary(), expected);
    let text = screen_at(&mut app, 200, 100);
    // The long header wraps inside the box: assert its pieces on the screen.
    for piece in [
        "Usage: sessions 183 of 1024 created in the last 24 h",
        "next slot frees at 2026-10-21 04:12 UTC",
        "841 left · 900 ever",
        "Created session window (hours)",
    ] {
        assert!(text.contains(piece), "{piece}\n{text}");
    }
}

// ---- #3417: operator provider spend grants -------------------------------

const OPENROUTER: usize = 3;

fn openrouter_grant() -> rsi_common::provider_grant::ManagerProviderGrantV2 {
    rsi_common::provider_grant::ManagerProviderGrantV2 {
        provider: SessionProvider::OpenRouter,
        budget_usd: 100.0,
        expires_at: Some(
            chrono::DateTime::parse_from_rfc3339("2026-12-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
        efforts: vec!["high".into(), "medium".into()],
        enable_bench: true,
        granted_at: Some(
            chrono::DateTime::parse_from_rfc3339("2026-10-20T18:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    }
}

/// The screenshot state with a saved OpenRouter grant and a loaded usage read
/// that reports $12.40 of $100 spent and `bench.enabled` still off.
async fn granted_app() -> App {
    let (mut app, mut config) = fixture();
    config.scope_mode = HarnessManagerScopeModeV1::Project;
    config.row_version = 93;
    config.epic_ids.clear();
    let mut saved = screenshot_policy(&config);
    saved.policy.provider_grants = vec![openrouter_grant()];
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
    state.preview_open = false;
    let status = openrouter_grant().status(
        12.4,
        chrono::DateTime::parse_from_rfc3339("2026-10-21T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    );
    state.apply_usage(Ok(Ok(policy::usage::CreationUsage {
        created: 183,
        created_limit: Some(1024),
        created_remaining: Some(841),
        window_hours: None,
        next_free_at: None,
        lifetime_total: None,
        active: 3,
        grants: vec![status],
        bench_enabled: Some(false),
    })));
    app
}

/// The live line the operator asked for, beside the grant's edit rows, and the
/// `bench.enabled` setting next to it.
#[tokio::test]
async fn the_policy_screen_shows_the_provider_grant_with_live_spend_and_bench() {
    let mut app = granted_app().await;
    let rows = policy_mut(&mut app).rows();
    let value = |field: policy::Field| {
        rows.iter()
            .find(|r| r.field == field)
            .unwrap_or_else(|| panic!("{field:?} is listed"))
            .value
            .clone()
    };
    assert_eq!(
        value(policy::Field::GrantStatus(OPENROUTER)),
        "OpenRouter: $12.40 of $100 used, expires 2026-12-01"
    );
    assert_eq!(value(policy::Field::GrantBudget(OPENROUTER)), "100");
    assert_eq!(
        value(policy::Field::GrantEfforts(OPENROUTER)),
        "high, medium"
    );
    assert_eq!(value(policy::Field::GrantExpiry(OPENROUTER)), "2026-12-01");
    assert_eq!(value(policy::Field::GrantBench(OPENROUTER)), "true");
    assert!(
        value(policy::Field::BenchSetting).starts_with("off"),
        "bench.enabled is shown next to the grant"
    );
    insta::assert_snapshot!("policy_provider_grant_120", screen_at(&mut app, 120, 120));
}

/// One save carries the whole grant: a budget entry creates it with the
/// default effort and the bench tick, the other rows edit it, and a blank
/// budget removes it. The daemon-assigned baseline is not an edit.
#[tokio::test]
async fn editing_a_provider_grant_is_one_draft_one_save() {
    let mut app = screenshot_app().await;
    let state = policy_mut(&mut app);
    let listed = |state: &policy::PolicyState| {
        state
            .rows()
            .iter()
            .filter(|r| {
                matches!(
                    r.field,
                    policy::Field::GrantBudget(_)
                        | policy::Field::GrantEfforts(_)
                        | policy::Field::GrantExpiry(_)
                        | policy::Field::GrantBench(_)
                )
            })
            .count()
    };
    assert_eq!(listed(state), 0, "collapsed with no grant");
    state.activate(policy::Field::ProviderGrants);
    assert_eq!(
        listed(state),
        9,
        "one budget row per provider when expanded"
    );
    state.activate(policy::Field::ProviderGrants);

    state
        .apply_text(policy::Field::GrantBudget(OPENROUTER), "100")
        .unwrap();
    assert_eq!(listed(state), 4, "budget, efforts, expiry and bench rows");
    let grant = state.request().policy.provider_grants[0].clone();
    assert_eq!(grant.provider, SessionProvider::OpenRouter);
    assert_eq!(grant.budget_usd, 100.0);
    assert_eq!(grant.efforts, vec!["high".to_string()]);
    assert!(
        grant.enable_bench,
        "an OpenRouter grant ticks bench by default"
    );
    assert_eq!(grant.granted_at, None, "the daemon assigns the baseline");
    assert!(state.modified(policy::Field::GrantBudget(OPENROUTER)));

    state
        .apply_text(policy::Field::GrantEfforts(OPENROUTER), "high, medium")
        .unwrap();
    state
        .apply_text(policy::Field::GrantExpiry(OPENROUTER), "2026-12-01")
        .unwrap();
    state.activate(policy::Field::GrantBench(OPENROUTER));
    let grant = state.request().policy.provider_grants[0].clone();
    assert_eq!(
        grant.efforts,
        vec!["high".to_string(), "medium".to_string()]
    );
    assert_eq!(grant.expires_at, openrouter_grant().expires_at);
    assert!(!grant.enable_bench);
    assert_eq!(state.request().policy.validate(), Ok(()));

    // Out-of-range input is refused on the row, not at save.
    assert!(
        state
            .apply_text(policy::Field::GrantBudget(OPENROUTER), "0")
            .is_err()
    );
    assert!(
        state
            .apply_text(policy::Field::GrantEfforts(OPENROUTER), "")
            .is_err()
    );
    assert!(
        state
            .apply_text(policy::Field::GrantExpiry(OPENROUTER), "next week")
            .is_err()
    );

    state
        .apply_text(policy::Field::GrantBudget(OPENROUTER), "")
        .unwrap();
    assert!(state.request().policy.provider_grants.is_empty());
    assert!(!state.modified(policy::Field::GrantBudget(OPENROUTER)));
}

/// A saved grant opened and not touched is not a modified row, whatever
/// baseline the daemon stamped on it.
#[tokio::test]
async fn an_unedited_saved_grant_shows_no_modified_marker() {
    let mut app = granted_app().await;
    let state = policy_mut(&mut app);
    for field in [
        policy::Field::GrantBudget(OPENROUTER),
        policy::Field::GrantEfforts(OPENROUTER),
        policy::Field::GrantExpiry(OPENROUTER),
        policy::Field::GrantBench(OPENROUTER),
    ] {
        assert!(!state.modified(field), "{field:?}");
    }
    state
        .apply_text(policy::Field::GrantBudget(OPENROUTER), "150")
        .unwrap();
    assert!(state.modified(policy::Field::GrantBudget(OPENROUTER)));
    let row = state
        .rows()
        .into_iter()
        .find(|r| r.field == policy::Field::GrantBudget(OPENROUTER))
        .unwrap();
    assert_eq!(row.saved.as_deref(), Some("100"));
}
