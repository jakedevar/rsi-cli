//! Tests of manager_ops.rs: the planners against the TUI's rules and the
//! forwarders' wire shapes, including a fake daemon socket.

use super::*;
use rsi_common::harness_manager_v2::ManagerOperatingModeV2;
use rsi_common::manager_nodes::{
    ManagerNodeAllowanceV1, ManagerNodeGrantStateV1, ManagerNodeGrantV1, ManagerNodeSelectorV1,
    ManagerNodeStateV1,
};

const P1: &str = "e2cae1d4-1833-48eb-a7e8-6f1dd480fdae";
const P2: &str = "0b7c8a52-5f63-4a9e-9d27-3c1a9a5b6e10";
const S1: &str = "7d0f4c2e-9a1b-4c3d-8e5f-6a7b8c9d0e1f";
const S2: &str = "2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d";
const N1: &str = "3b4c5d6e-7f8a-4b9c-8d1e-2f3a4b5c6d7e";
const N2: &str = "4c5d6e7f-8a9b-4c0d-9e1f-3a4b5c6d7e8f";

/// A fixed RFC 3339 timestamp, in whatever type the field holds.
fn at<T: serde::de::DeserializeOwned>() -> T {
    serde_json::from_value(json!("2026-10-18T00:00:00.000000000Z")).unwrap()
}

fn id(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

fn grant(state: &str, projects: &[&str]) -> GlobalManagerGrantV1 {
    GlobalManagerGrantV1 {
        grant_id: id(N1),
        grant_version: 3,
        seat_session_id: id(S1),
        state: state.into(),
        project_ids: projects.iter().map(|p| id(p)).collect(),
        allowed_launches: vec![parse_launch("Claude/claude-opus-5-5/high").unwrap()],
        project_policy: default_project_policy(),
        operator_origin: "operator".into(),
        created_at: at(),
        updated_at: at(),
    }
}

fn node(node_id: &str, state: &str, parent: Option<&str>, projects: &[&str]) -> PortfolioNodeV1 {
    PortfolioNodeV1 {
        node_id: id(node_id),
        tier_label: "global".into(),
        state: state.into(),
        authority_epoch: 2,
        parent_node_id: parent.map(id),
        grantor: "operator".into(),
        seat_root_session_id: id(S1),
        max_direct_reports: 5,
        child_policy: None,
        grant: grant(state, projects),
        created_at: at(),
        updated_at: at(),
    }
}

fn v<T: serde::Serialize>(x: &T) -> Value {
    serde_json::to_value(x).unwrap()
}

#[test]
fn defaults_are_the_execute_preset_and_the_operator_launches() {
    let p = default_project_policy();
    assert_eq!(p.mode, ManagerOperatingModeV2::Execute);
    assert!(p.validate().is_ok());
    assert_eq!(default_allowed_launches().len(), 10);
    let g = default_grant();
    assert_eq!(g["launches"][0]["model"], "claude-opus-5-5");
    assert_eq!(g["policy"]["mode"], "execute");
}

#[test]
fn a_first_appointment_uses_the_defaults_over_the_named_projects() {
    let out = global_appoint_plan(
        &Value::Null,
        &json!({"nodes": []}),
        Some(S2),
        &[P1.into()],
        false,
    )
    .unwrap();
    let r = &out["request"];
    assert_eq!(r["session_id"], S2);
    assert_eq!(r["project_ids"], json!([P1]));
    assert_eq!(r["expected_grant_version"], 0);
    assert_eq!(r["allowed_launches"].as_array().unwrap().len(), 10);
    assert_eq!(out["note"], "");
}

#[test]
fn replacing_an_active_seat_keeps_its_launches_and_policy() {
    let current = v(&grant("active", &[P1, P2]));
    let out = global_appoint_plan(
        &current,
        &json!({"nodes": []}),
        Some(S2),
        &[P1.into(), P2.into()],
        false,
    )
    .unwrap();
    assert_eq!(out["request"]["expected_grant_version"], 3);
    assert_eq!(
        out["request"]["allowed_launches"].as_array().unwrap().len(),
        1
    );
    let preview =
        global_appoint_plan(&current, &json!({"nodes": []}), None, &[P1.into()], false).unwrap();
    assert_eq!(preview["request"], Value::Null);
    assert_eq!(
        preview["note"],
        "A grant is active: appointing keeps its policy (nothing to prefill)."
    );
}

#[test]
fn after_a_revoke_the_last_revoked_root_is_the_prefill_unless_defaults() {
    let current = v(&grant("revoked", &[P1]));
    let nodes = json!({ "nodes": [v(&node(N1, "revoked", None, &[P1]))] });
    let out = global_appoint_plan(&current, &nodes, Some(S2), &[P1.into()], false).unwrap();
    assert_eq!(
        out["request"]["allowed_launches"].as_array().unwrap().len(),
        1
    );
    let note = out["note"].as_str().unwrap();
    assert!(
        note.starts_with(". Prefilled from grant v3 of revoked node 3b4c5d6e (same coverage)"),
        "{note}"
    );
    assert!(note.contains("launches +0 () -9"), "{note}");
    let defaults = global_appoint_plan(&current, &nodes, Some(S2), &[P1.into()], true).unwrap();
    assert_eq!(
        defaults["request"]["allowed_launches"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    assert!(
        defaults["note"]
            .as_str()
            .unwrap()
            .contains("first-appointment defaults")
    );
}

#[test]
fn set_changes_one_cap_and_keeps_the_rest() {
    let g = v(&grant("active", &[P1]));
    let out = global_set_plan(&g, "active", "12").unwrap();
    assert_eq!(out["request"]["project_policy"]["max_active_sessions"], 12);
    assert_eq!(out["request"]["expected_grant_version"], 3);
    assert_eq!(out["request"]["project_ids"], json!([P1]));
    assert!(
        out["change"]
            .as_str()
            .unwrap()
            .starts_with("max active sessions ")
    );
    assert_eq!(
        global_set_plan(&g, "spend", "none").unwrap()["request"]["project_policy"]["max_spend_usd"],
        Value::Null
    );
    assert_eq!(
        global_set_plan(&g, "groups", "off").unwrap()["request"]["project_policy"]["allow_create_groups"],
        false
    );
    // May create Groups needs the topology capability the Execute preset lacks.
    assert!(global_set_plan(&g, "groups", "on").is_err());
    assert!(global_set_plan(&g, "active", "0").is_err());
    assert!(global_set_plan(&g, "bogus", "1").is_err());
    assert!(global_set_plan(&Value::Null, "active", "3").is_err());
    assert!(global_set_plan(&v(&grant("revoked", &[P1])), "active", "3").is_err());
}

#[test]
fn add_projects_and_the_tree_edit_keep_the_grant_fenced() {
    let g = v(&grant("active", &[P1]));
    let r = global_add_projects_plan(&g, &[P2.into(), P1.into()]).unwrap();
    assert_eq!(r["project_ids"], json!([P1, P2]));
    assert_eq!(r["expected_grant_version"], 3);
    let caps = json!({"max_active_sessions": 7, "max_created_sessions": 64, "max_created_containers": 4, "max_spend_usd": 25.0, "allow_create_groups": false});
    let e = global_edit_plan(&g, None, &[P2.into()], Some(&caps)).unwrap();
    assert_eq!(e["project_ids"], json!([P2]));
    assert_eq!(e["project_policy"]["max_active_sessions"], 7);
    assert_eq!(e["project_policy"]["max_spend_usd"], 25.0);
    assert_eq!(e["session_id"], S1);
    let seat = global_edit_plan(&g, Some(S2), &[P1.into()], None).unwrap();
    assert_eq!(seat["session_id"], S2);
    assert!(global_edit_plan(&g, None, &[], None).is_err());
}

#[test]
fn launches_parse_like_the_tui() {
    let l = parse_launch("codex/gpt-6-astra/xhigh").unwrap();
    assert_eq!(l.provider, SessionProvider::Codex);
    assert_eq!(l.model, "gpt-6-astra");
    assert_eq!(l.effort.as_deref(), Some("xhigh"));
    let nested = parse_launch("OpenRouter/openai/gpt-x").unwrap();
    assert_eq!(nested.model, "openai/gpt-x");
    assert_eq!(nested.effort, None);
    assert!(parse_launch("nope/x").is_err());
    assert!(parse_launch("Claude").is_err());
    assert!(parse_launch("Claude/").is_err());
}

#[test]
fn portfolio_appoint_prefills_a_root_or_covers_adopted_nodes() {
    let nodes = json!({ "nodes": [v(&node(N1, "active", None, &[P1])), v(&node(N2, "revoked", None, &[P2]))] });
    let root = portfolio_appoint_plan(&nodes, "pinnacle", S2, &[P2.into()], &[]).unwrap();
    assert_eq!(root["request"]["tier_label"], "pinnacle");
    assert_eq!(root["request"]["seat_session_id"], S2);
    assert!(
        root["note"]
            .as_str()
            .unwrap()
            .starts_with("Prefilled from grant v3 of revoked node 4c5d6e7f")
    );
    let above = portfolio_appoint_plan(&nodes, "pinnacle", S2, &[P2.into()], &[N1.into()]).unwrap();
    let r = &above["request"];
    assert_eq!(r["adopt_node_ids"], json!([N1]));
    assert_eq!(r["project_ids"], json!([P1, P2]));
    let child_active = default_project_policy().max_active_sessions;
    assert_eq!(r["policy"]["max_active_sessions"], child_active + 1);
    assert_eq!(above["note"], Value::Null);
    assert!(portfolio_appoint_plan(&nodes, " padded ", S2, &[P2.into()], &[]).is_err());
    assert!(portfolio_appoint_plan(&nodes, "x", S2, &[], &[]).is_err());
    assert!(portfolio_appoint_plan(&nodes, "x", S2, &[], &[N2.into()]).is_err());
}

#[test]
fn portfolio_launch_edits_fence_on_the_grant_version() {
    let n = v(&node(N1, "active", None, &[P1]));
    let add = portfolio_launch_plan(&n, true, &["Codex/gpt-6-astra/high".into()]).unwrap();
    assert_eq!(add["node_id"], N1);
    assert_eq!(add["expected_node_grant_version"], 3);
    assert_eq!(add["add"][0]["model"], "gpt-6-astra");
    assert_eq!(add["remove"], json!([]));
    let rm = portfolio_launch_plan(&n, false, &["Claude/claude-opus-5-5/high".into()]).unwrap();
    assert_eq!(rm["remove"][0]["provider"], "Claude");
    assert!(portfolio_launch_plan(&n, true, &[]).is_err());
}

#[test]
fn tree_edits_on_portfolio_rows() {
    let n = v(&node(N1, "active", None, &[P1]));
    let seat = portfolio_edit_plan(&n, Some(S2), None, None).unwrap();
    assert_eq!(seat["seat_session_id"], S2);
    assert_eq!(seat["node_id"], N1);
    assert_eq!(seat["expected_node_grant_version"], 3);
    assert_eq!(seat["expected_authority_epoch"], 2);
    let caps = json!({"max_active_sessions": 9, "max_created_sessions": 10, "max_created_containers": 2, "max_spend_usd": null, "allow_create_groups": false});
    let edit = portfolio_edit_plan(&n, None, Some(&[P1.into(), P2.into()]), Some(&caps)).unwrap();
    assert_eq!(edit["project_ids"], json!([P1, P2]));
    assert_eq!(edit["policy"]["max_active_sessions"], 9);
    assert_eq!(edit["seat_session_id"], S1);
    assert!(
        portfolio_edit_plan(&v(&node(N1, "revoked", None, &[P1])), Some(S2), None, None).is_err()
    );
    let above = portfolio_above_plan(&n, S2).unwrap();
    assert_eq!(above["label"], "pinnacle");
    assert_eq!(above["request"]["adopt_node_ids"], json!([N1]));
    assert_eq!(label_above("pinnacle"), "swarm");
    assert_eq!(label_above("area"), "above area");
}

#[test]
fn a_move_widens_the_sibling_to_adopt_the_node() {
    let target = v(&node(N1, "active", None, &[P1]));
    let moved = v(&node(N2, "active", None, &[P2]));
    let out = portfolio_move_plan(&target, &moved).unwrap();
    assert_eq!(out["request"]["adopt_node_ids"], json!([N2]));
    assert_eq!(out["request"]["project_ids"], json!([P1, P2]));
    assert_eq!(out["added"], json!([P2]));
    let other = v(&node(N2, "active", Some(N1), &[P2]));
    assert!(portfolio_move_plan(&target, &other).is_err());
    assert!(portfolio_move_plan(&target, &target).is_err());
}

fn area(node_id: &str, parent: Option<&str>, reports: u16, active: u16) -> ManagerNodeViewV1 {
    // The node's policy mirrors its grant (ConfigureManagerNodeRequestV1::validate).
    let mut policy = ManagerPolicyV2::default();
    policy.max_created_containers = 0;
    policy.max_spend_usd = None;
    policy.max_active_sessions = active;
    policy.max_created_sessions = 8;
    ManagerNodeViewV1 {
        node_id: id(node_id),
        parent_node_id: parent.map(id),
        seat_root_session_id: id(S1),
        project_id: id(P1),
        selector: Some(ManagerNodeSelectorV1::Project),
        state: ManagerNodeStateV1::Active,
        grant_state: ManagerNodeGrantStateV1::Granted,
        grant: Some(ManagerNodeGrantV1 {
            capabilities: Vec::new(),
            allowed_launches: Vec::new(),
            allowance: ManagerNodeAllowanceV1 {
                max_created_containers: 0,
                max_created_sessions: 8,
                max_active_sessions: active,
                max_build_slots: 0,
                max_disk_gib: 0,
                provider_limits: Vec::new(),
                max_spend_usd: None,
            },
            max_direct_reports: 2,
        }),
        policy: Some(policy),
        grant_version: 4,
        policy_version: 6,
        authority_epoch: 1,
        direct_reports: reports,
        updated_at: at(),
    }
}

#[test]
fn an_area_edit_carves_the_allowance_under_both_fences() {
    let parent = v(&area(N1, None, 1, 10));
    let child = v(&area(N2, Some(N1), 0, 4));
    let r = node_edit_plan(&child, &parent, [3, 6, 1]).unwrap();
    assert_eq!(r["node_id"], N2);
    assert_eq!(r["parent_node_id"], N1);
    assert_eq!(r["grant"]["allowance"]["max_active_sessions"], 3);
    assert_eq!(r["policy"]["max_active_sessions"], 3);
    assert_eq!(r["grant"]["max_direct_reports"], 1);
    assert_eq!(r["expected_parent_grant_version"], 4);
    assert_eq!(r["expected_parent_policy_version"], 6);
    assert_eq!(r["expected_parent_authority_epoch"], 1);
    assert_eq!(r["expected_node_grant_version"], 4);
    assert!(node_edit_plan(&child, &parent, [4, 8, 2]).is_err());
    assert!(node_edit_plan(&v(&area(N2, Some(N1), 1, 4)), &parent, [3, 6, 1]).is_err());
}

#[test]
fn presets_classify_and_suggestions_come_from_rsi_common() {
    let base = v(&ManagerPolicyV2::default());
    let observe = policy_preset(&base, "observe", "project", false).unwrap();
    assert_eq!(observe["mode"], "monitor");
    let c = policy_classify(&observe, "project").unwrap();
    assert_eq!(c["preset"], "Observe");
    let exec = policy_preset(&base, "execute", "project", false).unwrap();
    assert_eq!(
        policy_classify(&exec, "project").unwrap()["preset"],
        "Execute"
    );
    assert!(policy_preset(&base, "nope", "project", false).is_err());
    assert!(policy_classify(&base, "everything").is_err());
    let mut zeroed: ManagerPolicyV2 = serde_json::from_value(exec.clone()).unwrap();
    zeroed.max_created_sessions = 0;
    let zeroed = v(&zeroed);
    let c = policy_classify(&zeroed, "project").unwrap();
    if c["suggestions"].as_array().is_some_and(|s| !s.is_empty()) {
        let fixed = policy_suggestions(&zeroed, "project").unwrap();
        assert!(fixed["max_created_sessions"].as_u64().unwrap() > 0);
    }
    assert!(policy_check(&exec).is_ok());
    let mut bad = exec;
    bad["max_active_sessions"] = json!(0);
    assert!(policy_check(&bad).is_err());
}

#[test]
fn a_launched_pm_is_appointed_with_its_policy_in_one_request() {
    let r = pm_appoint_plan(P1, S2, 3, 0, None).unwrap();
    assert_eq!(
        r["scope"],
        json!({"project_id": P1, "session_id": S2, "expected_row_version": 3})
    );
    assert_eq!(r["expected_policy_version"], 0);
    assert_eq!(r["policy"]["mode"], "execute");
    assert!(pm_appoint_plan("x", S2, 0, 0, None).is_err());
}

#[test]
fn forwarder_params_are_the_typed_requests() {
    let g = global_add_projects_plan(&v(&grant("active", &[P1])), &[P2.into()]).unwrap();
    let p = global_configure_params(&g, true).unwrap();
    assert_eq!(p["confirm_cap_reductions"], true);
    assert_eq!(p["project_ids"], json!([P1, P2]));
    assert!(
        global_configure_params(&g, false)
            .unwrap()
            .get("confirm_cap_reductions")
            .is_none()
    );
    let mut extra = g.clone();
    extra["surprise"] = json!(1);
    assert!(global_configure_params(&extra, false).is_err());
    let n = v(&node(N1, "active", None, &[P1]));
    let pr = portfolio_edit_plan(&n, Some(S2), None, None).unwrap();
    assert_eq!(
        portfolio_configure_full_params(&pr, false).unwrap()["seat_session_id"],
        S2
    );
    let launches = portfolio_launch_plan(&n, true, &["Codex/gpt-6-astra".into()]).unwrap();
    assert_eq!(
        portfolio_launches_params(&launches, true).unwrap()["confirm_cap_reductions"],
        true
    );
    assert_eq!(
        nodes_list_params(P1, None).unwrap(),
        json!({"project_id": P1, "after_node_id": null, "limit": 64})
    );
    assert_eq!(
        rule_params(N1, " split it ", "k").unwrap(),
        json!({"hop_id": N1, "ruling": "split it", "idempotency_key": "k"})
    );
    assert!(rule_params(N1, "  ", "k").is_err());
    assert_eq!(
        undelivered_params(Some("c1")).unwrap(),
        json!({"include_closed": false, "undelivered_after": "c1"})
    );
    assert_eq!(
        undelivered_params(None).unwrap()["undelivered_after"],
        Value::Null
    );
    assert!(undelivered_params(Some("a b")).is_err());
    assert_eq!(
        node_workspace_params("portfolio", N1).unwrap(),
        json!({"node": {"kind": "portfolio", "node_id": N1}})
    );
    assert_eq!(
        node_workspace_params("project", P1).unwrap(),
        json!({"node": {"kind": "project", "project_id": P1}})
    );
    assert!(node_workspace_params("global", N1).is_err());
    let parent = v(&area(N1, None, 1, 10));
    let child = v(&area(N2, Some(N1), 0, 4));
    let req = node_edit_plan(&child, &parent, [3, 6, 1]).unwrap();
    assert_eq!(node_configure_params(&req).unwrap()["node_id"], N2);
}

#[cfg(unix)]
#[tokio::test]
async fn each_forwarder_sends_its_method_and_params_to_the_daemon() {
    use crate::daemon::DaemonClient;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = Arc::clone(&seen);
    let g = global_add_projects_plan(&v(&grant("active", &[P1])), &[P2.into()]).unwrap();
    let n = v(&node(N1, "active", None, &[P1]));
    let calls = vec![
        (
            METHOD_CONFIGURE_GLOBAL,
            global_configure_params(&g, false).unwrap(),
        ),
        (
            METHOD_CONFIGURE_PORTFOLIO,
            portfolio_configure_full_params(
                &portfolio_edit_plan(&n, Some(S2), None, None).unwrap(),
                true,
            )
            .unwrap(),
        ),
        (
            METHOD_PORTFOLIO_LAUNCHES,
            portfolio_launches_params(
                &portfolio_launch_plan(&n, true, &["Codex/gpt-6-astra".into()]).unwrap(),
                false,
            )
            .unwrap(),
        ),
        (METHOD_LIST_NODES, nodes_list_params(P1, None).unwrap()),
        (METHOD_RESTART_STATUS, json!({})),
        (METHOD_RESTART_FORCE, json!({})),
        (METHOD_RESTART_CANCEL, json!({})),
        (METHOD_RULE, rule_params(N1, "r", "k").unwrap()),
        (METHOD_ACK, json!({"message_id": N2})),
        (METHOD_ESCALATIONS, undelivered_params(Some("c")).unwrap()),
        (
            METHOD_NODE_WORKSPACE,
            node_workspace_params("area", N1).unwrap(),
        ),
        (
            METHOD_WITH_POLICY,
            pm_appoint_plan(P1, S2, 0, 0, None).unwrap(),
        ),
    ];
    let count = calls.len();
    tokio::spawn(async move {
        for _ in 0..count {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut line = String::new();
            BufReader::new(r).read_line(&mut line).await.unwrap();
            seen2.lock().unwrap().push(line);
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
                .await
                .unwrap();
        }
    });
    let client = DaemonClient::new(&sock);
    for (method, params) in &calls {
        client.call(method, params.clone()).await.unwrap();
    }
    let sent: Vec<Value> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(sent.len(), count);
    for (i, (method, params)) in calls.iter().enumerate() {
        assert_eq!(sent[i]["method"], *method);
        assert_eq!(sent[i]["params"], *params);
    }
}
