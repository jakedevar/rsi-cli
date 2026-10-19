//! #2649: standing operator directives: a parent manager relays an operator
//! instruction, the seat of that scope reads it verbatim in its authority
//! catalog and overview, the global overview lists it per project, and the
//! write path stays operator-attributed.

use super::*;
use crate::session::agent_verbs::tests::{control_handle_with_store, test_session};
use crate::store::Store;
use rsi_common::agent_authority_catalog::{
    AgentAuthorityCatalogFieldsV1, AgentGetAuthorityCatalogRequestV1,
};
use rsi_common::global_manager::{AgentGlobalOverviewRequestV1, ConfigureGlobalManagerRequestV1};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
use rsi_common::manager_node_workspace::AgentManagerOverviewRequestV1;
use rsi_common::manager_tier_routing::ManagerNodeRefV1;
use rsi_common::standing_directives::{
    STANDING_DIRECTIVE_NOT_AUTHORIZED, StandingDirectiveActionV1,
};
use rsi_common::types::{Project, SessionKind, SessionProvider, SessionStatus};
use std::path::PathBuf;
use std::sync::Arc;

type SharedStore = Arc<tokio::sync::Mutex<Store>>;

const OVERNIGHT: &str = "Overnight: keep the hub and the laptop busy; resolve blocked runs on the wake that reports them; many small landings, reds first; escalate only real gates via the operator to-do list; send ONE morning summary.";

struct Fixture {
    control: AgentControlHandle,
    store: SharedStore,
    project: Uuid,
    other_project: Uuid,
    pm: Uuid,
    other_pm: Uuid,
    /// The portfolio (global) seat covering both projects.
    seat: Uuid,
    worker: Uuid,
}

fn add_project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    store
        .insert_project(&Project {
            id,
            name: name.into(),
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

fn add_session(store: &Store, project: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/standing-directives"));
    row.project_id = project;
    row.session_kind = SessionKind::Standard;
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    id
}

fn appoint(store: &Store, project: Uuid, session: Uuid) {
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: session,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .unwrap();
}

async fn fixture() -> Fixture {
    let (control, store) = control_handle_with_store();
    let guard = store.lock().await;
    let project = add_project(&guard, "Alpha");
    let other_project = add_project(&guard, "Beta");
    let pm = add_session(&guard, Some(project));
    let other_pm = add_session(&guard, Some(other_project));
    let worker = add_session(&guard, Some(project));
    let seat = add_session(&guard, None);
    appoint(&guard, project, pm);
    appoint(&guard, other_project, other_pm);
    guard
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![project, other_project],
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: Some("high".into()),
                }],
                project_policy: ManagerPolicyV2::default(),
                expected_grant_version: 0,
                idempotency_key: "grant-0".into(),
            },
            "operator:test",
        )
        .unwrap();
    drop(guard);
    Fixture {
        control,
        store,
        project,
        other_project,
        pm,
        other_pm,
        seat,
        worker,
    }
}

fn set(scope: ManagerNodeRefV1, text: &str, key: &str) -> AgentStandingDirectiveRequestV1 {
    AgentStandingDirectiveRequestV1 {
        action: StandingDirectiveActionV1::Set,
        scope: Some(scope.as_ref_text()),
        text: Some(text.into()),
        provenance: Some("operator, 2026-10-15 07:35Z, relayed by the global manager".into()),
        expires_in_secs: Some(8 * 3600),
        idempotency_key: Some(key.into()),
        directive_id: None,
    }
}

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

async fn guidance(f: &Fixture, caller: Uuid) -> String {
    f.control
        .agent_get_authority_catalog(caller, AgentGetAuthorityCatalogRequestV1::default())
        .await
        .unwrap()
        .guidance
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn a_relayed_directive_is_in_the_pm_catalog_and_overview_not_in_a_worker_or_other_pm() {
    let f = fixture().await;
    let project = ManagerNodeRefV1::Project {
        project_id: f.project,
    };
    let relayed = f
        .control
        .agent_standing_directive(f.seat, set(project, OVERNIGHT, "overnight-1"))
        .await
        .unwrap();
    assert!(!relayed.deduplicated);
    assert_eq!(relayed.directive.author, format!("session:{}", f.seat));

    // The PM seat reads every term verbatim, with who said it and when.
    let pm_guidance = guidance(&f, f.pm).await;
    assert!(pm_guidance.contains(OVERNIGHT), "{pm_guidance}");
    assert!(
        pm_guidance.contains("## Standing operator directives"),
        "{pm_guidance}"
    );
    assert!(
        pm_guidance.contains("relayed by the global manager"),
        "{pm_guidance}"
    );
    assert!(
        pm_guidance.contains(&format!("session:{}", f.seat)),
        "{pm_guidance}"
    );
    assert!(pm_guidance.contains("expires"), "{pm_guidance}");
    // Another project's PM, a worker and a single-verb request carry none.
    for other in [f.other_pm, f.worker] {
        assert!(
            !guidance(&f, other).await.contains(OVERNIGHT),
            "{other} must not receive another scope's directive"
        );
    }
    let single = f
        .control
        .agent_get_authority_catalog(
            f.pm,
            AgentGetAuthorityCatalogRequestV1 {
                verb: Some("AgentManagerOverview".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!single.guidance.contains(OVERNIGHT));

    // The PM's own overview lists it.
    let overview = f
        .control
        .agent_manager_overview(f.pm, AgentManagerOverviewRequestV1::default())
        .await
        .unwrap();
    assert_eq!(
        overview.standing_directives,
        vec![relayed.directive.clone()]
    );
    // The global overview lists it on that project and on no other.
    let global = f
        .control
        .agent_global_overview(f.seat, AgentGlobalOverviewRequestV1 {})
        .await
        .unwrap();
    let row = |id: Uuid| {
        global
            .projects
            .iter()
            .find(|project| project.project_id == id)
            .unwrap()
    };
    assert_eq!(row(f.project).standing_directives, vec![relayed.directive]);
    assert!(row(f.other_project).standing_directives.is_empty());
    let value = serde_json::to_value(&global).unwrap();
    assert!(
        value.to_string().contains(OVERNIGHT),
        "the overview JSON carries the text"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn the_write_path_is_operator_attributed_and_bounded_to_scopes_below_the_seat() {
    let f = fixture().await;
    let project = ManagerNodeRefV1::Project {
        project_id: f.project,
    };
    // A PM cannot write its own scope; a worker holds no seat at all.
    for caller in [f.pm, f.other_pm, f.worker] {
        let error = f
            .control
            .agent_standing_directive(caller, set(project, "x", "k"))
            .await
            .unwrap_err();
        assert_eq!(code(error), STANDING_DIRECTIVE_NOT_AUTHORIZED, "{caller}");
    }
    // No provenance, no relay.
    let mut bare = set(project, "x", "k");
    bare.provenance = None;
    let error = f
        .control
        .agent_standing_directive(f.seat, bare)
        .await
        .unwrap_err();
    assert_eq!(code(error), "standing_directive_invalid_request");

    // The relaying seat clears what it relayed; the replay is deduplicated.
    let relayed = f
        .control
        .agent_standing_directive(f.seat, set(project, OVERNIGHT, "k1"))
        .await
        .unwrap();
    let clear = AgentStandingDirectiveRequestV1 {
        action: StandingDirectiveActionV1::Clear,
        scope: None,
        text: None,
        provenance: None,
        expires_in_secs: None,
        idempotency_key: None,
        directive_id: Some(relayed.directive.id),
    };
    let cleared = f
        .control
        .agent_standing_directive(f.seat, clear.clone())
        .await
        .unwrap();
    assert!(cleared.directive.cleared_at.is_some() && !cleared.deduplicated);
    assert!(
        f.control
            .agent_standing_directive(f.seat, clear)
            .await
            .unwrap()
            .deduplicated
    );
    assert!(!guidance(&f, f.pm).await.contains(OVERNIGHT));

    // The operator's directive: a manager cannot clear it.
    let operator = f
        .store
        .lock()
        .await
        .standing_directive_set(
            &crate::store::standing_directives::StandingDirectiveActor::Operator,
            &crate::store::standing_directives::StandingDirectiveSetInput {
                scope: project,
                text: "Operator's own.",
                provenance: None,
                expires_in_secs: None,
                idempotency_key: "op",
            },
            chrono::Utc::now(),
        )
        .unwrap()
        .directive;
    let error = f
        .control
        .agent_standing_directive(
            f.seat,
            AgentStandingDirectiveRequestV1 {
                action: StandingDirectiveActionV1::Clear,
                scope: None,
                text: None,
                provenance: None,
                expires_in_secs: None,
                idempotency_key: None,
                directive_id: Some(operator.id),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(error), STANDING_DIRECTIVE_NOT_AUTHORIZED);
    assert!(guidance(&f, f.pm).await.contains("Operator's own."));
}

/// #308: a bare `{}` is the summary view (one-line descriptions, same names and
/// guidance); `fields: "full"` keeps each control's complete description; a
/// `verb` request is unchanged (no controls list, no view marker).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn the_default_catalog_is_the_summary_and_full_is_opt_in_308() {
    let f = fixture().await;
    let ask = |request: AgentGetAuthorityCatalogRequestV1| {
        let control = f.control.clone();
        let pm = f.pm;
        async move {
            control
                .agent_get_authority_catalog(pm, request)
                .await
                .unwrap()
        }
    };
    let default = ask(AgentGetAuthorityCatalogRequestV1::default()).await;
    let full = ask(AgentGetAuthorityCatalogRequestV1 {
        fields: Some(AgentAuthorityCatalogFieldsV1::Full),
        ..Default::default()
    })
    .await;
    assert_eq!(default.view, "summary");
    assert!(full.view.is_empty());
    assert_eq!(default.guidance, full.guidance);
    assert_eq!(
        default
            .controls
            .iter()
            .map(|c| &c.method)
            .collect::<Vec<_>>(),
        full.controls.iter().map(|c| &c.method).collect::<Vec<_>>()
    );
    let size = |catalog: &rsi_common::agent_authority_catalog::AgentAuthorityCatalogV1| {
        serde_json::to_vec(catalog).unwrap().len()
    };
    assert!(size(&default) + 10_000 < size(&full));
    assert!(size(&default) <= 50_000, "{}", size(&default));
    let verb = ask(AgentGetAuthorityCatalogRequestV1 {
        verb: Some("AgentManagerOverview".into()),
        ..Default::default()
    })
    .await;
    assert!(verb.view.is_empty() && verb.controls.is_empty() && verb.control.is_some());
}
