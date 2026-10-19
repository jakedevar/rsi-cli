use std::sync::{Arc, Mutex};

use super::*;
use crate::client::DaemonClient;
use rsi_common::global_manager::GlobalManagerGrantV1;
use rsi_common::types::SessionProvider;
use serde_json::{Value, json};

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A one-connection fake daemon answering by method; every call is recorded.
async fn fake_daemon(
    mut handler: impl FnMut(&str, &Value) -> Result<Value, String> + Send + 'static,
) -> (App, Calls, tempfile::TempDir) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let directory = crate::test_support::short_socket_dir("rsi-pfcmd");
    let socket = directory.path().join("d.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let calls: Calls = Arc::default();
    let record = calls.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let request: Value = serde_json::from_str(&line).unwrap();
            let method = request["method"].as_str().unwrap_or_default().to_string();
            let params = request["params"].clone();
            record
                .lock()
                .unwrap()
                .push((method.clone(), params.clone()));
            let response = match handler(&method, &params) {
                Ok(result) => json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
                Err(message) => json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32000, "message": message},
                }),
            };
            if write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    app.client = DaemonClient::new(socket);
    app.client.connect().await.unwrap();
    app.poll.connected = true;
    (app, calls, directory)
}

fn params_of(calls: &Calls, method: &str) -> Value {
    calls
        .lock()
        .unwrap()
        .iter()
        .find(|(name, _)| name == method)
        .map(|(_, params)| params.clone())
        .unwrap_or_else(|| panic!("{method} was not called"))
}

fn project(name: &str) -> Project {
    let now = chrono::Utc::now();
    Project {
        id: Uuid::new_v4(),
        name: name.into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: now,
        updated_at: now,
    }
}

fn node(label: &str, seat: Uuid, version: i64, epoch: i64, projects: Vec<Uuid>) -> PortfolioNodeV1 {
    let now = chrono::Utc::now();
    PortfolioNodeV1 {
        node_id: Uuid::new_v4(),
        tier_label: label.into(),
        state: "active".into(),
        authority_epoch: epoch,
        parent_node_id: None,
        grantor: "operator".into(),
        seat_root_session_id: seat,
        max_direct_reports: 5,
        child_policy: None,
        grant: GlobalManagerGrantV1 {
            grant_id: Uuid::new_v4(),
            grant_version: version,
            seat_session_id: seat,
            state: "active".into(),
            project_ids: projects,
            allowed_launches: default_allowed_launches(),
            project_policy: default_project_policy(),
            operator_origin: "operator_rpc".into(),
            created_at: now,
            updated_at: now,
        },
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn portfolio_command_parses_each_verb() {
    assert_eq!(parse(""), Ok(PortfolioCommand::List));
    assert_eq!(parse("list"), Ok(PortfolioCommand::List));
    assert_eq!(
        parse("show global"),
        Ok(PortfolioCommand::Show("global".into()))
    );
    assert_eq!(
        parse("appoint pinnacle Rsi, Notes"),
        Ok(PortfolioCommand::Appoint {
            label: "pinnacle".into(),
            projects: "Rsi, Notes".into(),
            adopt: Vec::new(),
        })
    );
    assert_eq!(
        parse("appoint global"),
        Ok(PortfolioCommand::Appoint {
            label: "global".into(),
            projects: String::new(),
            adopt: Vec::new(),
        })
    );
    assert_eq!(
        parse("appoint pinnacle --adopt 1234abcd,global Extra"),
        Ok(PortfolioCommand::Appoint {
            label: "pinnacle".into(),
            projects: "Extra".into(),
            adopt: vec!["1234abcd".into(), "global".into()],
        })
    );
    assert_eq!(
        parse("appoint swarm Rsi --adopt=pinnacle"),
        Ok(PortfolioCommand::Appoint {
            label: "swarm".into(),
            projects: "Rsi".into(),
            adopt: vec!["pinnacle".into()],
        })
    );
    assert_eq!(
        parse("appoint swarm --adopt").unwrap_err(),
        "--adopt needs <node,...>"
    );
    assert_eq!(
        parse("configure {\"a\":1}"),
        Ok(PortfolioCommand::Configure("{\"a\":1}".into()))
    );
    assert_eq!(
        parse("revoke 1234abcd"),
        Ok(PortfolioCommand::Revoke("1234abcd".into()))
    );
    assert_eq!(parse("revoke").unwrap_err(), USAGE);
    assert_eq!(parse("appoint").unwrap_err(), USAGE);
    assert_eq!(
        parse(&format!("appoint {}", "x".repeat(33))).unwrap_err(),
        format!("Invalid tier label: {}", "x".repeat(33))
    );
}

#[test]
fn portfolio_node_resolves_by_id_prefix_or_unique_label() {
    let seat = Uuid::new_v4();
    let first = node("global", seat, 1, 1, vec![]);
    let second = node("global", seat, 2, 2, vec![]);
    let pinnacle = node("pinnacle", seat, 3, 3, vec![]);
    let nodes = vec![first.clone(), second.clone(), pinnacle.clone()];
    assert_eq!(
        resolve_node(&nodes, &first.node_id.to_string()),
        Ok(first.node_id)
    );
    assert_eq!(
        resolve_node(&nodes, &second.node_id.to_string()[..8]),
        Ok(second.node_id)
    );
    assert_eq!(resolve_node(&nodes, "pinnacle"), Ok(pinnacle.node_id));
    assert_eq!(
        resolve_node(&nodes, "global").unwrap_err(),
        "global names 2 portfolio nodes; use the node id"
    );
}

/// `:manager portfolio appoint` sends a root with the focused seat, the named
/// projects and the operator defaults; `show` and `revoke` read the node and
/// revoke it at its current versions.
#[tokio::test]
async fn portfolio_commands_round_trip_through_the_rpcs() {
    let (rsi, notes, other) = (project("Rsi"), project("Notes"), project("Other"));
    let seat = Uuid::new_v4();
    let appointed = node("pinnacle", seat, 4, 4, vec![rsi.id, notes.id]);
    let node_id = appointed.node_id;
    let mut revoked = appointed.clone();
    revoked.state = "revoked".into();
    revoked.grant.state = "revoked".into();
    let (listed, got, saved, gone) = (
        serde_json::to_value(vec![appointed.clone()]).unwrap(),
        serde_json::to_value(&appointed).unwrap(),
        serde_json::to_value(&appointed).unwrap(),
        serde_json::to_value(&revoked).unwrap(),
    );
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "ListPortfolioNodes" => Ok(json!({ "nodes": listed.clone() })),
        "GetPortfolioNode" => Ok(got.clone()),
        "ConfigurePortfolioNode" => Ok(saved.clone()),
        "RevokePortfolioNode" => Ok(gone.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.projects = vec![rsi.clone(), notes.clone(), other.clone()];
    let focused = app.selected_session_id().expect("a focused session");

    let message = run(&mut app, "appoint pinnacle Rsi, Notes").await.unwrap();
    assert!(
        message.starts_with("Portfolio node appointed: pinnacle"),
        "{message}"
    );
    let sent = params_of(&calls, "ConfigurePortfolioNode");
    assert_eq!(sent["node_id"], Value::Null);
    assert_eq!(sent["parent_node_id"], Value::Null);
    assert_eq!(sent["tier_label"], "pinnacle");
    assert_eq!(sent["seat_session_id"], focused.to_string());
    assert_eq!(
        sent["project_ids"],
        json!([rsi.id.to_string(), notes.id.to_string()])
    );
    assert_eq!(sent["expected_node_grant_version"], 0);
    assert_eq!(sent["expected_authority_epoch"], 0);

    let shown = run(&mut app, "show pinnacle").await.unwrap();
    assert!(shown.contains("v4 epoch 4"), "{shown}");
    assert_eq!(
        params_of(&calls, "GetPortfolioNode")["node_id"],
        node_id.to_string()
    );

    let message = run(&mut app, &format!("revoke {node_id}")).await.unwrap();
    assert!(message.contains("(revoked)"), "{message}");
    let sent = params_of(&calls, "RevokePortfolioNode");
    assert_eq!(sent["node_id"], node_id.to_string());
    assert_eq!(sent["expected_grant_version"], 4);
    assert_eq!(sent["expected_authority_epoch"], 4);
}

/// `appoint` without project names covers every project no active node holds.
#[tokio::test]
async fn portfolio_appoint_defaults_to_uncovered_projects() {
    let (rsi, notes) = (project("Rsi"), project("Notes"));
    let held = node("global", Uuid::new_v4(), 2, 2, vec![rsi.id]);
    let saved = node("pinnacle", Uuid::new_v4(), 3, 3, vec![notes.id]);
    let (listed, saved) = (
        serde_json::to_value(vec![held]).unwrap(),
        serde_json::to_value(saved).unwrap(),
    );
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "ListPortfolioNodes" => Ok(json!({ "nodes": listed.clone() })),
        "ConfigurePortfolioNode" => Ok(saved.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.projects = vec![rsi, notes.clone()];
    run(&mut app, "appoint pinnacle").await.unwrap();
    assert_eq!(
        params_of(&calls, "ConfigurePortfolioNode")["project_ids"],
        json!([notes.id.to_string()])
    );
}

/// #1902: a new root after a revoke starts from the most recent revoked root
/// grant over the same coverage; other coverage starts from the defaults.
#[tokio::test]
async fn portfolio_appoint_prefills_from_the_last_revoked_root_grant() {
    use rsi_common::harness_manager_v2::ManagerCapabilityV2;
    let (rsi, notes) = (project("Rsi"), project("Notes"));
    let mut revoked = node("global", Uuid::new_v4(), 8, 3, vec![rsi.id]);
    revoked.state = "revoked".into();
    revoked.grant.state = "revoked".into();
    revoked.max_direct_reports = 12;
    revoked.grant.project_policy.max_active_sessions = 20;
    revoked.grant.project_policy.capabilities = vec![ManagerCapabilityV2::Deploy];
    let launches = revoked.grant.allowed_launches.len();
    let saved = node("global", Uuid::new_v4(), 1, 1, vec![rsi.id]);
    let (listed, saved) = (
        serde_json::to_value(vec![revoked.clone()]).unwrap(),
        serde_json::to_value(saved).unwrap(),
    );
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "ListPortfolioNodes" => Ok(json!({ "nodes": listed.clone() })),
        "ConfigurePortfolioNode" => Ok(saved.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.projects = vec![rsi.clone(), notes.clone()];

    let message = run(&mut app, "appoint global Rsi").await.unwrap();
    let sent = params_of(&calls, "ConfigurePortfolioNode");
    assert_eq!(sent["policy"]["max_active_sessions"], 20);
    assert_eq!(sent["policy"]["capabilities"], json!(["deploy"]));
    assert_eq!(sent["max_direct_reports"], 12);
    assert_eq!(sent["allowed_launches"].as_array().unwrap().len(), launches);
    assert!(message.contains("Prefilled from grant v8"), "{message}");
    assert!(message.contains("+deploy"), "{message}");

    // Other coverage shares no project with the revoked grant: defaults.
    let message = run(&mut app, "appoint global Notes").await.unwrap();
    let sent = calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(name, _)| name == "ConfigurePortfolioNode")
        .map(|(_, params)| params.clone())
        .expect("the second appointment");
    assert_eq!(
        sent["policy"],
        serde_json::to_value(default_project_policy()).unwrap()
    );
    assert_eq!(sent["max_direct_reports"], 5);
    assert!(!message.contains("Prefilled"), "{message}");
}

/// #1237: `appoint <label> --adopt <node,...>` appoints a manager above the
/// adopted roots: it covers their projects plus the named ones, sits where
/// they sat, and every adopted node narrows its grant.
#[tokio::test]
async fn portfolio_appoint_adopt_sends_a_grant_every_adopted_node_narrows() {
    use rsi_common::grant_narrowing::{grant_narrows, portfolio_bounds, portfolio_grant_bounds};
    let (rsi, notes, extra) = (project("Rsi"), project("Notes"), project("Extra"));
    let first = node("global", Uuid::new_v4(), 2, 2, vec![rsi.id]);
    let mut second = node("region", Uuid::new_v4(), 3, 3, vec![notes.id]);
    second.grant.project_policy.max_active_sessions = 40;
    second.grant.project_policy.max_created_sessions = 99;
    second.max_direct_reports = 9;
    let saved = node("pinnacle", Uuid::new_v4(), 4, 4, vec![rsi.id, notes.id]);
    let (listed, saved) = (
        serde_json::to_value(vec![first.clone(), second.clone()]).unwrap(),
        serde_json::to_value(saved).unwrap(),
    );
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "ListPortfolioNodes" => Ok(json!({ "nodes": listed.clone() })),
        "ConfigurePortfolioNode" => Ok(saved.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.projects = vec![rsi.clone(), notes.clone(), extra.clone()];
    let message = run(&mut app, "appoint pinnacle --adopt global,region Extra")
        .await
        .unwrap();
    assert!(
        message.starts_with("Portfolio node appointed above 2 node(s)"),
        "{message}"
    );
    let sent: ConfigurePortfolioNodeRequestV1 =
        serde_json::from_value(params_of(&calls, "ConfigurePortfolioNode")).unwrap();
    assert_eq!(sent.adopt_node_ids, [first.node_id, second.node_id]);
    assert_eq!(sent.parent_node_id, None);
    assert_eq!(sent.project_ids, [rsi.id, notes.id, extra.id]);
    assert_eq!(sent.validate(), Ok(()));
    let bounds = portfolio_bounds(
        &sent.project_ids,
        &sent.allowed_launches,
        &sent.policy,
        sent.max_direct_reports,
    );
    for child in [&first, &second] {
        assert_eq!(
            grant_narrows(
                &portfolio_grant_bounds(&child.grant, child.max_direct_reports),
                &bounds
            ),
            Ok(()),
            "{}",
            child.tier_label
        );
    }
}

#[test]
fn portfolio_launch_command_parses_provider_model_and_effort() {
    let fable = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-fable-5-1".into(),
        effort: Some("max".into()),
    };
    assert_eq!(
        parse("launch add global Claude/claude-fable-5-1/max"),
        Ok(PortfolioCommand::Launch {
            add: true,
            node: "global".into(),
            launches: vec![fable.clone()],
        })
    );
    assert_eq!(
        parse("launch remove 1234abcd claude/claude-fable-5-1/max codex/gpt-6-astra"),
        Ok(PortfolioCommand::Launch {
            add: false,
            node: "1234abcd".into(),
            launches: vec![
                fable,
                ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Codex,
                    model: "gpt-6-astra".into(),
                    effort: None,
                },
            ],
        })
    );
    // A trailing segment that is not an effort belongs to the model id.
    assert_eq!(
        parse_launch("OpenRouter/anthropic/claude-x").unwrap().model,
        "anthropic/claude-x"
    );
    assert!(parse("launch add global").is_err());
    assert!(parse("launch add global nonsense").is_err());
    assert!(parse("launch grant global Claude/m").is_err());
}

/// #2103: one launch is added through `ConfigurePortfolioNodeLaunches` at the
/// node's current grant version, never by re-typing the grant.
#[tokio::test]
async fn portfolio_launch_add_sends_one_cas_fenced_launch_edit() {
    let seat = Uuid::new_v4();
    let current = node("global", seat, 13, 4, vec![]);
    let node_id = current.node_id;
    let mut saved = current.clone();
    saved.grant.grant_version = 14;
    let (listed, got, saved) = (
        serde_json::to_value(vec![current.clone()]).unwrap(),
        serde_json::to_value(current).unwrap(),
        serde_json::to_value(saved).unwrap(),
    );
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "ListPortfolioNodes" => Ok(json!({ "nodes": listed.clone() })),
        "GetPortfolioNode" => Ok(got.clone()),
        "ConfigurePortfolioNodeLaunches" => Ok(saved.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    let message = run(&mut app, "launch add global Claude/claude-fable-5-1/max")
        .await
        .unwrap();
    assert!(
        message.starts_with("Portfolio launches updated"),
        "{message}"
    );
    let sent = params_of(&calls, "ConfigurePortfolioNodeLaunches");
    assert_eq!(sent["node_id"], node_id.to_string());
    assert_eq!(sent["expected_node_grant_version"], 13);
    assert_eq!(
        sent["add"],
        json!([{"provider": "Claude", "model": "claude-fable-5-1", "effort": "max"}])
    );
    assert_eq!(sent["remove"], json!([]));
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _)| method != "ConfigurePortfolioNode")
    );
}
