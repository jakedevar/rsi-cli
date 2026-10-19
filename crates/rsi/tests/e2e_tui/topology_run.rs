//! Topology run view (#1641 S7, run view #1746/#1753): the operator starts the
//! starter `issue-implement-review-land` on a focused Issue with
//! `:topology run`, through the real `ExecuteTopology` RPC, and reads the run
//! in the graph review's run view.
//!
//! A provider-backed implement/review/land pipeline is out of scope and would
//! make the run non-deterministic. `ExecuteTopology` only accepts the starter
//! (its command and land nodes) while the executor is on, so the test lets the
//! real accept happen, switches the executor off (`topology_executor_enabled`,
//! a live setting), waits for the run to go quiet, and replaces the attempt and
//! event rows of that throw-away execution with the ones a mid-loop run would
//! have, plus an answered ruling. The run view reads them back through the
//! real `GetWorkflowExecution` snapshot when the graph review is reopened.
use super::manager_decisions::{Fixture, setup};
use super::*;
use rsi_common::harness_manager_v2::*;
use serde_json::{Value, json};

const STARTER: &str = "issue-implement-review-land";

async fn press(term: &Terminal, key: Key) -> E2eResult<()> {
    term.send_key(key)
        .await
        .map(|_| ())
        .map_err(|e| test_error(format!("topology run key: {e:?}")))
}

async fn visible(term: &Terminal, text: &str) -> E2eResult<()> {
    term.expect(text)
        .timeout(Duration::from_secs(15))
        .await
        .map_err(|e| test_error(format!("expected {text:?}: {e:?}")))
}

async fn command(term: &Terminal, text: &str) -> E2eResult<()> {
    for c in format!(":{text}").chars() {
        press(term, Key::Char(c)).await?;
    }
    press(term, Key::Enter).await
}

fn stamp(offset_secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(offset_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// Topology custody starts from `origin/rolling`: give the fixture repo an
/// `origin` (a local bare repo) that carries its `rolling` branch.
fn add_origin(h: &E2eHarness, tag: &str) -> E2eResult<()> {
    let repo = h.home_dir.join(format!("{tag}-repo"));
    let origin = h.home_dir.join(format!("{tag}-origin.git"));
    for args in [
        vec!["init".into(), "--bare".into(), origin.display().to_string()],
        vec![
            "remote".into(),
            "add".into(),
            "origin".into(),
            origin.display().to_string(),
        ],
        vec!["push".into(), "origin".into(), "rolling".into()],
        vec!["fetch".into(), "origin".into()],
    ] {
        let mut git = Command::new("git");
        if args[0] != "init" {
            git.arg("-C").arg(&repo);
        }
        let out = git.args(&args).output()?;
        if !out.status.success() {
            return Err(test_error(format!(
                "fixture git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
    }
    Ok(())
}

/// Give the fixture manager what `ExecuteTopology` needs from an operator
/// run's owner: the Automation capability, the Epic in scope, execute mode.
async fn authorise_operator_runs(fixture: &mut Fixture) -> E2eResult<()> {
    let config = fixture
        .client
        .get_harness_manager(fixture.project_id)
        .await?
        .ok_or_else(|| test_error("manager config missing"))?;
    fixture
        .client
        .configure_harness_manager_policy(ConfigureHarnessManagerPolicyRequestV2 {
            project_id: fixture.project_id,
            expected_scope_version: config.row_version,
            expected_policy_version: 1,
            idempotency_key: "topology-run-policy".into(),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::Automation,
                ],
                ..Default::default()
            },
        })
        .await?;
    fixture.scope_version = config.row_version;
    Ok(())
}

/// Wait until the freshly accepted run has stopped changing: the executor is
/// off, so any in-flight round finishes and nothing new is written.
async fn settle_executor(fixture: &Fixture) -> E2eResult<()> {
    let conn = rusqlite::Connection::open(&fixture.db)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let mut previous = String::new();
    let mut stable = 0;
    for _ in 0..60 {
        let now: String = conn.query_row(
            "SELECT (SELECT COUNT(*) || MAX(updated_at) FROM topology_node_attempts) || '|' || \
                    (SELECT COUNT(*) FROM topology_events) || '|' || \
                    (SELECT row_version || status FROM topology_executions)",
            [],
            |r| r.get(0),
        )?;
        stable = if now == previous { stable + 1 } else { 0 };
        if stable >= 4 {
            return Ok(());
        }
        previous = now;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(test_error("the accepted topology run never went quiet"))
}

/// Write the rows a run that is mid-loop would have: implement done, review
/// asked for a fix, fix done, review back for round two and waiting on the
/// on-call manager; one ruling already answered. `land` never started.
fn seed_run(fixture: &Fixture) -> E2eResult<()> {
    let conn = rusqlite::Connection::open(&fixture.db)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let (execution, workflow, topology): (String, String, Option<String>) = conn.query_row(
        "SELECT id,workflow_id,topology_id FROM topology_executions",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    // The execution is the test's own; whatever the executor wrote before it
    // was switched off is replaced so the run is exactly the story below.
    conn.execute(
        "DELETE FROM topology_events WHERE execution_id=?1",
        [&execution],
    )?;
    conn.execute(
        "DELETE FROM topology_node_attempts WHERE execution_id=?1",
        [&execution],
    )?;
    conn.execute(
        "UPDATE topology_executions SET status='running',started_at=?2,finished_at=NULL,\
         error=NULL,blocked_reason_json=NULL,output_json=NULL,lease_owner=NULL,\
         lease_expires_at=NULL,row_version=row_version+1,updated_at=?2 WHERE id=?1",
        rusqlite::params![execution, stamp(0)],
    )?;
    // (node, kind, iteration, attempt_no, status)
    for (node, kind, iteration, attempt, status) in [
        ("implement", "session", 0, 1, "succeeded"),
        ("review", "review", 0, 1, "succeeded"),
        ("fix", "session", 0, 1, "succeeded"),
        ("review", "review", 1, 1, "waiting"),
    ] {
        conn.execute(
            "INSERT INTO topology_node_attempts \
             (id,execution_id,node_id,iteration,attempt_no,node_kind,status,dedup_key,\
              session_id,input_json,created_at,updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?10,'{}',?9,?9)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                execution,
                node,
                iteration,
                attempt,
                kind,
                status,
                format!("topology-run-e2e-{node}-{iteration}-{attempt}"),
                stamp(0),
                uuid::Uuid::new_v4().to_string(),
            ],
        )?;
    }
    let mut seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(execution_seq),0) FROM topology_events WHERE execution_id=?1",
        [&execution],
        |r| r.get(0),
    )?;
    let mut event = |kind: &str, node: &str, state: Option<&str>, detail: Value| -> E2eResult<()> {
        seq += 1;
        let now = stamp(seq);
        let update = json!({
            "execution_id": execution, "workflow_id": workflow, "node_id": node,
            "sequence": seq, "status": "running", "node_state": state,
            "finished": false, "error": null, "output_preview": null,
            "updated_at": now,
        });
        conn.execute(
            "INSERT INTO topology_events \
             (execution_id,execution_seq,topology_id,node_id,kind,actor_kind,payload_json,created_at) \
             VALUES (?1,?2,?3,?4,?5,'executor',?6,?7)",
            rusqlite::params![
                execution,
                seq,
                topology,
                node,
                kind,
                json!({ "update": update, "detail": detail }).to_string(),
                now
            ],
        )?;
        Ok(())
    };
    for (node, state) in [
        ("implement", "running"),
        ("implement", "succeeded"),
        ("review", "running"),
        ("review", "succeeded"),
        ("fix", "running"),
        ("fix", "succeeded"),
        ("review", "running"),
    ] {
        event("node_state", node, Some(state), Value::Null)?;
    }
    event(
        "on_call_unavailable",
        "review",
        None,
        json!({ "reason": "no_live_project_manager", "on_call": { "kind": "project_manager" } }),
    )?;
    let payload = json!({
        "key": format!("topology:{execution}:review-0"), "epic_id": fixture.epic,
        "question": "Ship it?", "request_id": null, "work_key": null,
        "target_digest": "digest-topology-run", "target_row_version": null,
        "status": "answered", "answer": "yes, land it", "delivery": null,
        "asked_by": { "node_id": "review" }, "answered_by": { "kind": "manager" },
    });
    conn.execute(
        "INSERT INTO harness_manager_v2_records \
         (project_id,manager_session_id,scope_version,kind,record_key,epic_id,row_version,\
          payload_json,archived,created_at,updated_at) \
         VALUES (?1,?2,?3,'decision',?4,?5,1,?6,0,?7,?7)",
        rusqlite::params![
            fixture.project_id.to_string(),
            fixture.manager.id.to_string(),
            fixture.scope_version,
            format!("topology:{execution}:review-0"),
            fixture.epic.to_string(),
            payload.to_string(),
            stamp(0),
        ],
    )?;
    Ok(())
}

async fn open_term(h: &E2eHarness) -> E2eResult<Terminal> {
    Terminal::builder()
        .size(150, 46)
        .env("HOME", &h.path_string(&h.home_dir)?)
        .env("RSI_DAEMON_SOCKET_PATH", &h.path_string(&h.socket_path)?)
        .env("RSI_TUI_NO_AUTO_START_DAEMON", "1")
        .env("RSI_SESSION_TOKEN", "")
        .env("PATH", &h.isolated_path)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("LANG", "C.UTF-8")
        .spawn(&h.rsi_bin, &[])
        .await
        .map_err(|e| test_error(format!("topology run TUI: {e:?}")))
}

/// Space, `p`, pick the fixture project, then Space, `i`: the project's Issues
/// workspace with the seeded Local Issue focused.
async fn open_issues_workspace(term: &Terminal, project: &str, issue: &str) -> E2eResult<()> {
    press(term, Key::Char(' ')).await?;
    press(term, Key::Char('p')).await?;
    visible(term, "Workspaces").await?;
    visible(term, project).await?;
    press(term, Key::Char('j')).await?;
    press(term, Key::Enter).await?;
    visible(term, "Decisions Group").await?;
    press(term, Key::Char(' ')).await?;
    press(term, Key::Char('i')).await?;
    visible(term, issue).await
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_e2e_topology_run_view_shows_rows_ruling_and_the_waiting_banner() -> E2eResult<()> {
    if std::env::var("RSI_E2E").unwrap_or_default() != "1" {
        return Ok(());
    }
    let mut h = E2eHarness::new()?;
    let mut fixture = setup(&mut h, "topology-run").await?;
    let issue_title = "Run the starter topology";
    let outcome_setup: E2eResult<()> = async {
        add_origin(&h, "topology-run")?;
        authorise_operator_runs(&mut fixture).await?;
        fixture
            .client
            .create_issue_v2(CreateIssueV2RequestV1 {
                project_id: fixture.project_id,
                title: issue_title.to_string(),
                body: "Implement, review and land this Issue".to_string(),
                priority: Some(2),
                labels: vec!["e2e".to_string()],
                assignee: Some("operator".to_string()),
                idempotency_key: "e2e-topology-run-issue".to_string(),
            })
            .await?;
        Ok(())
    }
    .await;
    if let Err(error) = outcome_setup {
        h.shutdown_daemon();
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; topology run artifacts: {}",
            artifacts.display()
        )));
    }
    let term = open_term(&h).await?;
    let outcome = async {
        visible(&term, "Decisions manager").await?;
        open_issues_workspace(&term, "Decisions fixture", issue_title).await?;

        // The real start: ExecuteTopology accepts the run and the run view
        // opens on it. Every node of the starter is a row, none has run yet.
        command(&term, &format!("topology run {STARTER}")).await?;
        visible(&term, &format!("Graph Review: {STARTER}")).await?;
        visible(&term, "implement  ").await?;
        for node in ["review", "fix", "land"] {
            visible(&term, &format!("{node}  pending")).await?;
        }

        // The run advances (rows the executor would have written), the
        // on-call manager goes away, and the operator reopens the view.
        fixture
            .client
            .update_daemon_config("topology_executor_enabled", json!(false))
            .await?;
        settle_executor(&fixture).await?;
        seed_run(&fixture)?;
        let manager_id = fixture.manager.id;
        let _ = fixture.client.interrupt_session(manager_id).await;
        press(&term, Key::Char('q')).await?;
        visible(&term, "Local").await?;
        command(&term, "graph").await?;

        visible(&term, &format!("{STARTER} running")).await?;
        visible(&term, "implement  succeeded  attempt 1 round 1").await?;
        visible(&term, "fix  succeeded  attempt 1 round 1").await?;
        visible(&term, "land  pending").await?;
        // The current node carries the marker and the second review round.
        visible(&term, "> review  running  attempt 1 round 2").await?;
        // The on-call manager is gone: the banner names the waiting node.
        visible(&term, "on-call: project manager").await?;
        visible(&term, "waiting: review - no_live_project_manager").await?;
        // The answered ruling is listed with who answered it.
        visible(&term, "rulings (1)").await?;
        visible(&term, "review [answered by manager] Ship it?").await?;
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    }
    .await;
    h.final_screen_text = Some(term.screen().await.text());
    let _ = term.send_key(Key::Ctrl('c')).await;
    h.shutdown_daemon();
    if let Err(error) = outcome {
        let artifacts = h.preserve_artifacts(Some(&error.to_string()))?;
        return Err(test_error(format!(
            "{error}; topology run artifacts: {}",
            artifacts.display()
        )));
    }
    Ok(())
}
